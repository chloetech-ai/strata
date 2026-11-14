// Versions (HARNESS.md, "Versions, promote and roll back"): the harness's
// history as a graph of versions, the diff of the candidate against the
// current one by part and as text, and Promote or Roll back.
import { lbl, btn, head, ic, lv, kc } from "./dom.js";
import { versions } from "./fixtures.js";

export function versionsPage() {
  const rows = versions.map((v) => `<tr class="${v.v === "1.5-rc" ? "cur" : ""}">
      <td class="gr"><span class="gd ${v.state || "old"}${v.branch ? " br" : ""}"></span></td>
      <td><b class="num">${v.v}</b> <span class="mut num">${v.sha}</span>${v.state ? ` <span class="vs ${v.state}">${v.state}</span>` : ""}</td>
      <td>${v.note}<div class="fa">${v.by} · ${v.from} · ${v.when}</div></td>
      <td class="r num">${v.sessions}</td><td class="r num">${v.success}</td><td class="r num dk">${v.prompts}</td></tr>`).join("");
  return {
    crumb: "shop|harness|versions",
    content: `${head({ kicker: "Optimize the harness · versions", title: "1.5-rc is ready to replace 1.4", sub: "A version is every part of the harness at once, by its hash. Each agent records the one it started with; promoting changes what new agents start with, and rolling back is one more version.", tabs: "versions" })}
      <section class="sec">${lbl("History", { n: versions.length, right: '<span class="mut">sessions · success · prompts a session</span>' })}
        <div class="pane"><table class="tbl vers">
          <tr><th></th><th>Version</th><th>Change</th><th class="r">Sessions</th><th class="r">Success</th><th class="r dk">Prompts</th></tr>
          ${rows}
        </table></div></section>
      <section class="sec">${lbl("1.4 → 1.5-rc", { n: "2 parts", right: '<div class="seg sm"><a class="on" href="#">By part</a><a href="#">Text</a></div>' })}
        <div class="pane">
          <div class="dpart"><div class="dph"><b>Permissions</b><span class="flag">${ic("tri")}widens</span><span class="sp"></span>${lv("counted")}<span class="mut num">−18 prompts in 7 days · 0 denies changed</span></div>
            <div class="hunk"><div class="mut"><span class="ln">.claude/settings.json · permissions.allow</span></div><div><span class="ln">8</span>  "Bash(git commit:*)",</div><div class="add"><span class="ln">9</span>+ "Bash(PYTHONPATH=. python3 -m pytest:*)",</div><div><span class="ln">10</span>  "Bash(python3 serve.py:*)",</div></div></div>
          <div class="dpart"><div class="dph"><b>Memory</b><span class="sp"></span>${lv("recomputed")}<span class="mut num">+5 records in 4 sessions · 2 misses fixed</span></div>
            <div class="hunk"><div class="mut"><span class="ln">chloectx · recall.toml</span></div><div><span class="ln">14</span>  [pins]</div><div><span class="ln">15</span>  "money.py" = ["a91c02f4:12"]</div><div class="add"><span class="ln">16</span>+ anchored_decisions = { by = "user", when = "file-edited" }</div></div></div>
          <div class="dpart off"><div class="dph"><b>Memory · briefing</b><span class="mut">left out: inconclusive in run 7</span></div></div>
        </div></section>`,
    insp: `${lbl("Promote 1.5", { strong: true })}
      <dl class="kv sm"><dt>New agents</dt><dd>start with 1.5 from now on</dd><dt>Running</dt><dd>freeship and paybutton stay on 1.4 until they restart</dd><dt>Approval</dt><dd><span class="t-warn">1 rule widens.</span> Promoting is your approval of it.</dd><dt>Watch</dt><dd>the next 10 sessions against 1.4's last 10</dd><dt>Back</dt><dd>Roll back makes 1.6 with 1.4's parts; nothing is deleted</dd></dl>
      <div>${lbl("Tested", { strong: true })}
        <ul class="safe">
          <li>${ic("check", "color:var(--good)")}<span>shop core: 11 of 12 pass³ (1.4: 8)</span></li>
          <li>${ic("check", "color:var(--good)")}<span>held out: 2 win, 2 tie, 0 lose</span></li>
          <li>${ic("check", "color:var(--good)")}<span>Claude Code 2.x, the same as 1.4's runs</span></li>
        </ul></div>
      <div class="q"><div class="hd"><b>Roll back</b><span class="t">to</span></div>
        <div class="opts"><a class="opt" href="#"><span class="kc">1</span>1.3 · Nov 8 · before the pytest rule</a><a class="opt" href="#"><span class="kc">2</span>1.2 · Nov 6</a><a class="opt" href="#"><span class="kc">3</span>Pick a version…</a></div></div>
      <div class="dock">${btn("Promote 1.5 to shop", { pri: true, k: "⌘↵" })}${btn("Branch from 1.4", { k: "B" })}</div>`,
    mdock: btn("Promote 1.5", { pri: true }),
  };
}
