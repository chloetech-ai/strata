// The harness editor (HARNESS.md, "The harness IDE"): one structured view
// per part, the raw text beside it in two-way sync, autocomplete from what
// the agents did, lint, and the impact of a change on the last 7 days.
import { lbl, btn, gauge, head, lv, ic, kc, ticks, st } from "./dom.js";

const PARTS = [["instructions", "Instructions", 6], ["prompts", "Prompts", 4], ["permissions", "Permissions", 47], ["hooks", "Hooks", 11], ["tools", "Tools", 3], ["routing", "Routing", 4], ["memory", "Memory", 9]];

function parts(on, lint) {
  return `<nav class="parts">${PARTS.map(([k, l, n]) => `<a class="${k === on ? "on" : ""}" href="#/harness/edit/${k === "prompts" || k === "instructions" ? "permissions" : k === "routing" ? "tools" : k}"><span>${l}</span><span class="num">${n}</span>${lint[k] ? `<span class="lint-n ${lint[k][1]}">${lint[k][0]}</span>` : ""}</a>`).join("")}</nav>`;
}
const LINT = { permissions: [4, "warn"], hooks: [1, "warn"], memory: [1, "bad"], tools: [1, ""] };

const view = (on) => `<div class="seg sm">${["Structured", "Split", "Text"].map((x) => `<a class="${x === on ? "on" : ""}" href="#" data-act="view">${x}</a>`).join("")}</div>`;

function frameHead(part, sub) {
  return `${head({ kicker: "Optimize the harness · editing 1.5-draft, from 1.4", title: part, sub, tabs: "edit" })}
    <div class="edbar">${parts(part.toLowerCase(), LINT)}</div>`;
}

// ------------------------------------------------------------ permissions
const RULES = [
  ["deny", "Bash(git push:*)", "guarded", "Workbench", 0, "–"],
  ["deny", "Bash(pip install:*)", "guarded", "you, Nov 3", 2, "denied 2"],
  ["deny", "Bash(rm -rf:*)", "guarded", "Workbench", 0, "–"],
  ["ask", "WebFetch", "", "Claude Code", 3, "asked 3"],
  ["allow", "Edit(&lt;worktree&gt;/**)", "worktree", "Workbench", 211, "07:43"],
  ["allow", "Read(&lt;worktree&gt;/**)", "worktree", "Workbench", 486, "07:43"],
  ["allow", "Bash(python3 -m pytest:*)", "worktree", "Workbench", 0, "–", "unreach"],
  ["allow", "Bash(git commit:*)", "worktree", "Workbench", 14, "07:39"],
  ["allow", "Bash(python3 serve.py:*)", "", "Flow, Nov 8 (1.3)", 6, "07:19"],
  ["allow", "Bash(python3:*)", "", "you, Nov 3", 41, "07:40", "wide"],
];

export function permissions() {
  const rows = RULES.map(([d, r, tag, from, hits, last, lint]) => `<tr class="${lint ? "lt-" + lint : ""}"><td><span class="pd ${d}">${d}</span></td><td><code>${r}</code>${lint === "unreach" ? `<span class="lflag">${ic("tri")}never matches</span>` : lint === "wide" ? `<span class="lflag">${ic("tri")}wider than needed</span>` : ""}</td><td class="mut">${from}</td><td class="r num">${hits}</td><td class="r num mut dk">${last}</td></tr>`).join("");
  const json = [
    ["", "{"], ["", '  "permissions": {'], ["", '    "deny": ['], ["", '      "Bash(git push:*)", "Bash(pip install:*)",'], ["", '      "Bash(rm -rf:*)"'], ["", "    ],"], ["", '    "ask": ["WebFetch"],'], ["", '    "allow": ['],
    ["", '      "Edit(/…/wb/freeship/**)",'], ["", '      "Read(/…/wb/freeship/**)",'], ["w", '      "Bash(python3 -m pytest:*)",'], ["", '      "Bash(git commit:*)",'], ["", '      "Bash(python3 serve.py:*)",'], ["w", '      "Bash(python3:*)",'],
    ["a", '      "Bash(PYTHONPATH=. python3 -m py▍'], ["", "    ]"], ["", "  }"], ["", "}"],
  ];
  return {
    crumb: "shop|harness|editor|permissions",
    content: `${frameHead("Permissions", "What agents of shop may do without asking, must ask for, or may never do. Hits are this week's tool calls each rule decided.")}
      <div class="edtools">${lbl("Rules", { n: 10, strong: true })}<span class="sp"></span><span class="mut" style="font-size:12px">deny wins over ask, ask over allow</span>${view("Split")}</div>
      <div class="split">
        <div class="pane">
          <table class="tbl rules">
            <tr><th>Decide</th><th>Rule</th><th>From</th><th class="r">Hits 7d</th><th class="r dk">Last</th></tr>
            ${rows}
            <tr class="new"><td><span class="pd allow">allow</span></td><td colspan="4"><div class="acbox"><code>Bash(PYTHONPATH=. python3 -m py<span class="caret"></span></code></div>
              <div class="ac" role="listbox">
                <div class="ach">${lbl("Ran here · 7 days", { strong: false, right: '<span class="mut">asked · allowed · denied</span>' })}</div>
                <a class="aci on" href="#"><code>PYTHONPATH=. python3 -m pytest:*</code><span class="acn num">22 · 22 · 0</span><span class="acw">cents, freeship, flakehunt · 9 sessions</span></a>
                <a class="aci" href="#"><code>PYTHONPATH=. python3 -m pytest -q tests/:*</code><span class="acn num">18 · 18 · 0</span><span class="acw">narrower: misses 4 run on one file</span></a>
                <a class="aci" href="#"><code>PYTHONPATH=. python3 -m pyflakes:*</code><span class="acn num">2 · 1 · 1</span><span class="acw">you denied it once (paybutton, Nov 13)</span></a>
                <div class="acf">${kc("↑")}${kc("↓")} pick ${kc("Tab")} take · only commands that ran here</div>
              </div></td></tr>
          </table>
        </div>
        <div class="pane code">
          <div class="hd"><span class="lbl" style="flex:1">settings.json · permissions</span><span class="sync">${ic("check")}in sync</span></div>
          <div class="src">${json.map(([k, t], i) => `<div class="${k}"><span class="ln">${i + 1}</span>${t.replace(/</g, "&lt;")}</div>`).join("")}</div>
          <div class="srcf">The block Workbench writes into each session's own <code>--settings</code>. Yours in <code>~/.claude</code> are read, never written.</div>
        </div>
      </div>`,
    insp: `${lbl("Lint", { n: 4 })}
      <div class="lint">
        <div class="li warn">${ic("tri")}<div><b>Never matches: <code>Bash(python3 -m pytest:*)</code></b><span>Agents type <code>PYTHONPATH=. python3 -m pytest</code>; a rule matches from the first word. 22 prompts this week.</span></div></div>
        <div class="li warn">${ic("tri")}<div><b>Wider than needed: <code>Bash(python3:*)</code></b><span>41 hits, 39 of them <code>-m pytest</code> or <code>serve.py</code>. It also allows <code>python3 -c "…"</code>, which ran twice. Narrow to 2 rules.</span></div></div>
        <div class="li">${ic("dia")}<div><b>Shadowed, yours: <code>allow Bash(git push:*)</code></b><span>In ~/.claude/settings.json; the deny here wins in Workbench's sessions.</span></div></div>
        <div class="li bad">${ic("x")}<div><b>A secret in a config</b><span><code>env.STRIPE_TEST_KEY = sk_test_…</code> in .claude/settings.local.json. Move it to the environment.</span></div></div>
      </div>
      <div>${lbl("Impact of the new rule", { right: lv("counted") })}
        <div class="bank two imp2" style="margin-top:10px">
          ${gauge({ label: "Prompts", v: "22 → 4", on: 4, total: 22, delta: "4 pipe into tail -n 30" })}
          ${gauge({ label: "Denies changed", v: "0", on: 0, total: 22, delta: "of 2 denies this week", dtone: "good" })}
        </div>
        <p class="note">Replayed against 1,284 tool calls of the last 7 days: 18 calls you allowed by hand would have run without asking, and 14m 50s of waiting would not have happened.</p>
        <div class="widen">${ic("tri")}<span><b>It widens what agents may do.</b> It is kept only with your approval, in every auto-learn mode.</span></div></div>
      <div class="dock">${btn("Save to 1.5-draft", { pri: true, k: "⌘S" })}${btn("Test it on 9 sessions", { href: "#/harness/run/7" })}</div>`,
    mdock: btn("Save to 1.5-draft", { pri: true }),
  };
}

// ------------------------------------------------------------ hooks
const HOOKS = [
  ["SessionStart", "chloectx", "chloectx hook", "", "5 s", "410 ms", "690 ms", 14, "1.1k", ""],
  ["SessionStart", "workbench", "chloedb-workbench hook", "", "5 s", "38 ms", "61 ms", 14, "180", ""],
  ["UserPromptSubmit", "chloectx", "chloectx hook", "", "5 s", "640 ms", "1.9 s", 96, "640", "slow"],
  ["UserPromptSubmit", "workbench", "chloedb-workbench hook", "", "5 s", "22 ms", "40 ms", 96, "60", ""],
  ["PreToolUse", "workbench", "chloedb-workbench hook", "*", "5 s", "9 ms", "18 ms", 1284, "210 ×4", ""],
  ["PostToolUse", "workbench", "chloedb-workbench hook", "*", "5 s", "11 ms", "25 ms", 1241, "–", ""],
  ["PostToolUse", "chloectx", "chloectx hook", "*", "5 s", "30 ms", "70 ms", 1241, "fixes ×3", ""],
  ["PostToolUse", "project", ".claude/hooks/ruff.sh", "Edit|Write", "30 s", "380 ms", "1.2 s", 211, "–", ""],
  ["Stop", "chloectx", "chloectx hook · distill after", "", "5 s", "15 ms", "30 ms", 96, "–", ""],
  ["Stop", "workbench", "chloedb-workbench hook", "", "5 s", "14 ms", "22 ms", 96, "–", ""],
  ["SessionEnd", "workbench", "chloedb-workbench hook", "", "5 s", "12 ms", "19 ms", 13, "–", ""],
];

export function hooks() {
  const wf = [
    ["SessionStart", 0, 1.2, "410"], ["UserPromptSubmit", 1.4, 3.6, "1,890", "slow"], ["model", 3.6, 21, ""], ["PreToolUse Edit", 21, 21.3, "9"], ["PostToolUse ruff.sh", 22, 23.2, "380"],
    ["model", 23.2, 38, ""], ["PreToolUse Bash", 38, 38.3, "11"], ["waiting on you", 38.3, 78, "permission", "wait"], ["Bash pytest", 78, 84, ""], ["PostToolUse", 84, 84.4, "30"], ["model", 84.4, 92, ""], ["Stop", 92, 92.4, "15"],
  ];
  const W = 94;
  return {
    crumb: "shop|harness|editor|hooks",
    content: `${frameHead("Hooks", "What runs at each moment of a turn, how long it takes, and what it adds to the context. Times are this week's runs, measured by Workbench.")}
      <section class="sec" style="margin-top:20px">${lbl("One turn, as it ran · freeship turn 2 · 94 s", { link: "Pick a turn" })}
        <div class="pane wfp">
          <div class="ruler"><span>0 s</span><span>20 s</span><span>40 s</span><span>60 s</span><span>80 s</span><span>94 s</span></div>
          ${wf.map(([n, a, b, ms, k]) => `<div class="wf ${k || ""}"><span class="wn">${n}</span><span class="wt"><i style="left:${(a / W) * 100}%;width:${Math.max(0.6, ((b - a) / W) * 100)}%"></i></span><span class="wm num">${ms ? ms + (k === "wait" ? "" : " ms") : ""}</span></div>`).join("")}
        </div></section>
      <section class="sec">${lbl("Hooks", { n: HOOKS.length, right: view("Structured") })}
        <div class="pane"><table class="tbl hk">
          <tr><th>When</th><th>Hook</th><th class="dk">Matcher</th><th class="r">p50</th><th class="r">p95</th><th class="r">Runs 7d</th><th class="r">Adds tokens</th></tr>
          ${HOOKS.map(([ev, who, cmd, m, to, p50, p95, runs, tok, k]) => `<tr class="${k === "slow" ? "cur" : ""}"><td><code>${ev}</code></td><td><b>${who}</b><div class="fa"><code>${cmd}</code> · timeout ${to}</div></td><td class="dk"><code>${m || "–"}</code></td><td class="r num">${p50}</td><td class="r num ${k ? "t-warn" : ""}">${p95}</td><td class="r num">${runs}</td><td class="r num">${tok}</td></tr>`).join("")}
        </table></div></section>`,
    insp: `${lbl("chloectx · UserPromptSubmit", { strong: true })}
      <dl class="kv sm"><dt>Command</dt><dd><code>~/.local/bin/chloectx hook</code></dd><dt>Does</dt><dd>Recall: searches the project's memory with the prompt, gates and packs it, writes a receipt.</dd><dt>Timeout</dt><dd><code>5 s</code> · in 1.4-hooks: <code>3 s</code></dd><dt>From</dt><dd><code>chloectx wiring</code>, merged by Workbench</dd></dl>
      <div>${lbl("Last runs", { n: 6 })}
        <div class="runs6">${[["07:42", "1,890", "640"], ["07:31", "610", "590"], ["07:24", "580", "720"], ["07:19", "2,140", "0"], ["07:14", "540", "410"], ["07:12", "3,020", "1.1k"]].map(([t, ms, tok]) => `<div><span class="num mut">${t}</span><span class="num ${parseInt(ms.replace(",", "")) > 1500 ? "t-warn" : ""}">${ms} ms</span><span class="num mut">${tok} tok</span><span class="num mut">exit 0</span></div>`).join("")}</div></div>
      <div class="lint"><div class="li warn">${ic("tri")}<div><b>Slow on every prompt</b><span>p95 1.9 s. Each of the 3 slow runs here was the first after the daemon slept: the embedder loads cold. Its time is in the turn, before the model starts.</span></div></div>
        <div class="li">${ic("dia")}<div><b>Under the limit</b><span>Its output never passed the 10,000 characters Claude Code keeps inline.</span></div></div></div>
      <div class="dock">${btn("Run it on turn 2 again", { pri: true, k: "R" })}${btn("Open its script", {})}</div>`,
    mdock: btn("Run it on turn 2 again", { pri: true }),
  };
}

// ------------------------------------------------------------ memory
export function memory() {
  const slider = (name, val, pos, from, note, tone = "") => `<div class="sl"><div class="slh"><span>${name}</span><b class="num ${tone}">${val}</b></div>
      <div class="slt">${ticks(0, 40, { mark: pos })}${from != null ? `<span class="was" style="left:${(from / 40) * 100}%"></span>` : ""}</div><div class="note">${note}</div></div>`;
  return {
    crumb: "shop|harness|editor|memory",
    content: `${frameHead("Memory", "What ChloeCtx gives an agent: the briefing it starts with, recall on each prompt, and records pinned to files. Every change is checked against this week's recalls, on the store as each one saw it.")}
      <div class="split mem">
        <div class="pane" style="padding:14px 16px">
          ${lbl("Recall", { strong: true, right: view("Structured") })}
          ${slider("Decisions by you: give at", "0.60", 24, null, "Records of your decisions under it are left out. 0.58 missed freeship's turn 2.")}
          ${slider("Knowledge: any record at", "0.40", 16, null, "Rules, decisions and facts that match the prompt on their own text.")}
          ${slider("Budget a prompt", "1,000 tok", 20, null, "4,000 characters; at most 12 records, 8 from one turn.")}
          ${slider("Briefing at session start", "1,000 tok", 20, null, "3 of its 9 notes were read back this week.")}
          ${lbl("Pins", { n: 2, strong: true, style: "margin-top:18px" })}
          <div class="pins">
            <div><code>money.py</code><span>“Money is integer cents everywhere” · 1.1, from Debug</span></div>
            <div class="add"><code>anchored file</code><span><b>New:</b> your decisions, given when an agent edits the file they name</span></div>
          </div>
        </div>
        <div class="pane prev">
          <div class="hd">${lbl("Live preview · freeship turn 2", { strong: true, link: "Pick a turn" })}</div>
          <div class="pv"><div class="lbl" style="margin-bottom:6px">Prompt</div><p>“Go ahead: shipping line first, with tests.”</p></div>
          <table class="tbl pvt">
            <tr><th>Record</th><th class="r">Score</th><th>1.4</th><th>1.5-draft</th></tr>
            <tr><td>Money is integer cents everywhere</td><td class="r num">0.91</td><td>given</td><td>given</td></tr>
            <tr><td>Customers win: everything a customer sees rounds half up</td><td class="r num">0.74</td><td>given</td><td>given</td></tr>
            <tr class="chg"><td><b>Shipping is not taxed: tax applies to the items only</b><div class="fa">decision · you · Nov 10 · anchored cart.py</div></td><td class="r num">0.58</td><td class="t-bad">left out</td><td class="t-good"><b>pinned</b></td></tr>
            <tr><td class="mut">test_promo_window is a known flaky test</td><td class="r num mut">0.31</td><td class="mut">left out</td><td class="mut">left out</td></tr>
          </table>
          <div class="pv"><div class="lbl" style="margin-bottom:8px">Every recall this week, again ${lv("recomputed")}</div>
            <div class="bank g3 sm">
              ${gauge({ label: "Recalls", v: "212", on: 24, total: 24, delta: "14 sessions" })}
              ${gauge({ label: "Records added", v: "+5", on: 5, total: 24, delta: "in 4 sessions, +38 tok a turn there" })}
              ${gauge({ label: "Misses fixed", v: "2", on: 2, total: 24, tone: "good", delta: "freeship t2, refunds t3", dtone: "good" })}
            </div></div>
        </div>
      </div>`,
    insp: `${lbl("Lint", { n: 1 })}
      <div class="lint"><div class="li bad">${ic("x")}<div><b>A decision was missed where it applied</b><span>freeship edited cart.py at turn 2; your decision about cart.py scored 0.58 and was left out. 2 tests failed after it.</span></div></div></div>
      <div>${lbl("Two ways to fix it", { strong: true })}
        <div class="fix on"><span class="box">${ic("check", "width:12px;height:12px")}</span><div><b>Pin your decisions to their files</b><span>+5 records this week, only on the files they name. Fixes both misses.</span></div></div>
        <div class="fix"><span class="box"></span><div><b>Give decisions by you at 0.50</b><span>+21 records this week, everywhere. Fixes both misses; 16 of the extra records were never read back.</span></div></div></div>
      <p class="note">The preview reruns ChloeCtx's recall from each receipt's inputs, on the snapshot of the store that receipt read. It is exact for what is given; whether the model then uses it takes a replay.</p>
      <div class="dock">${btn("Save to 1.5-draft", { pri: true, k: "⌘S" })}${btn("Test it on the 2 sessions", { href: "#/harness/run/7" })}</div>`,
    mdock: btn("Save to 1.5-draft", { pri: true }),
  };
}

// ------------------------------------------------------------ tools and routing
export function tools() {
  return {
    crumb: "shop|harness|editor|tools",
    content: `${frameHead("Tools", "MCP servers, skills and subagents an agent of shop starts with, what each costs in context before the first word, and what was used. Then which model gets which kind of task.")}
      <section class="sec" style="margin-top:20px">${lbl("MCP servers and skills", { n: 6 })}
        <div class="pane"><table class="tbl">
          <tr><th>Server</th><th>From</th><th class="r">Tools</th><th class="r">Tokens a turn</th><th class="r">Calls 14d</th><th>State</th></tr>
          <tr><td><b>workbench</b><div class="fa">spawn_agent, propose_change, ask_human, +11</div></td><td class="mut">Workbench</td><td class="r num">14</td><td class="r num">3.2k</td><td class="r num">212</td><td>${st("work", "on")}</td></tr>
          <tr><td><b>chloectx</b><div class="fa">memory_search, memory_ask, memory_remember, +6</div></td><td class="mut">chloectx wiring</td><td class="r num">9</td><td class="r num">2.4k</td><td class="r num">88</td><td>${st("work", "on")}</td></tr>
          <tr class="cur"><td><b>playwright</b><div class="fa">browser_navigate, browser_click, +16</div></td><td class="mut">~/.claude.json (yours)</td><td class="r num">18</td><td class="r num t-warn">6.1k</td><td class="r num t-warn">0</td><td>${st("", "on")} <span class="lflag">${ic("tri")}unused</span></td></tr>
          <tr><td><b>skill: release-notes</b><div class="fa">.claude/skills/release-notes</div></td><td class="mut">project</td><td class="r num">–</td><td class="r num">90</td><td class="r num">0</td><td>${st("", "on")}</td></tr>
          <tr><td><b>subagent: test-runner</b><div class="fa">.claude/agents/test-runner.md · haiku</div></td><td class="mut">project</td><td class="r num">–</td><td class="r num">140</td><td class="r num">23</td><td>${st("work", "on")}</td></tr>
        </table></div></section>
      <section class="sec">${lbl("Routing", { n: 4, right: '<span class="mut">from task_features: what finished, and its cost</span>' })}
        <div class="pane"><table class="tbl">
          <tr><th>Tasks like</th><th>Model</th><th>Effort</th><th class="r">Finished</th><th class="r">Cost a task</th><th class="dk">Learned from</th></tr>
          <tr><td>Read-only: explain, find</td><td><code>haiku</code></td><td><code>low</code></td><td class="r num">6 of 6</td><td class="r num">$0.04</td><td class="dk mut">explain, 5 more</td></tr>
          <tr><td>A failing test, one file</td><td><code>haiku</code></td><td><code>medium</code></td><td class="r num">5 of 6</td><td class="r num">$0.09</td><td class="dk mut">flakehunt, cents</td></tr>
          <tr class="cur"><td>Money, tax, totals</td><td><code>sonnet</code></td><td><code>high</code></td><td class="r num">4 of 5</td><td class="r num">$0.52</td><td class="dk mut">haiku finished 1 of 4</td></tr>
          <tr><td>Anything else</td><td><code>sonnet</code></td><td><code>medium</code></td><td class="r num">7 of 9</td><td class="r num">$0.38</td><td class="dk mut">default</td></tr>
        </table></div></section>`,
    insp: `${lbl("Lint", { n: 1 })}
      <div class="lint"><div class="li warn">${ic("tri")}<div><b>18 tools nobody called</b><span>playwright comes from your own ~/.claude.json and costs 6.1k tokens every turn of every agent of shop. 0 calls in 14 days.</span></div></div></div>
      <div class="fix on"><span class="box">${ic("check", "width:12px;height:12px")}</span><div><b>Leave playwright out of shop's agents</b><span>Workbench starts them with <code>--strict-mcp-config</code> and its own list. Your file is not touched.</span></div></div>
      <p class="note">Narrowing: it takes tools away, so it can be applied within limits without asking. An agent that needs it asks.</p>
      <div class="dock">${btn("Save to 1.5-draft", { pri: true, k: "⌘S" })}</div>`,
    mdock: btn("Save to 1.5-draft", { pri: true }),
  };
}
