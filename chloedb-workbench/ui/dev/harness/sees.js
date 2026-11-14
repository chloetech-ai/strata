// What the model sees (HARNESS.md, "Screens", 6): the context an agent had
// at a turn, assembled part by part, with each part's tokens and share, under
// the version it ran and under a candidate.
import { lbl, btn, head, lv, ic } from "./dom.js";

// [part, source, tokens 1.4, tokens 1.5-rc, measured|estimated, tone]
const PARTS = [
  ["Claude Code's system prompt", "Claude Code 2.x · not versioned here", 3100, 3100, "estimated", "s0"],
  ["Tool definitions", "built-in 4.2k · workbench 3.2k · chloectx 2.4k · playwright 6.1k", 15900, 15900, "measured", "s1"],
  ["CLAUDE.md", "~/.claude/CLAUDE.md 0.2k · shop/CLAUDE.md 0.4k", 600, 600, "measured", "s2"],
  ["Rules from Workbench", "2 rules · SessionStart", 180, 180, "measured", "s2"],
  ["ChloeCtx briefing", "9 notes · SessionStart", 1100, 1100, "measured", "ctx"],
  ["Task", "your words, played", 90, 90, "measured", "s2"],
  ["Recall on this prompt", "2 given → 3 given, 1 pinned", 640, 690, "measured", "ctx"],
  ["The conversation so far", "turn 1: 14 tool calls, their results", 4790, 4790, "measured", "s3"],
];
const total = (i) => PARTS.reduce((s, p) => s + p[i], 0);

function bar(i, label) {
  const t = total(i);
  return `<div class="tokrow"><span class="dvk">${label}</span><div class="tok">${PARTS.map((p) => `<i class="${p[5]}${i === 3 && p[3] !== p[2] ? " chg" : ""}" style="flex:${p[i]}" title="${p[0]}: ${p[i]} tokens"></i>`).join("")}</div><span class="num tokt">${(Math.round(t / 100) / 10).toFixed(1)}k · ${Math.round((t / 64000) * 100)}%</span></div>`;
}

export function sees() {
  const t = total(2);
  return {
    crumb: "shop|harness|what the model sees",
    content: `${head({ kicker: "Optimize the harness · freeship · turn 2 · harness 1.4", title: "What freeship saw at turn 2", sub: "26.4k tokens before it wrote a word. A quarter of it was tools it never called; the decision that mattered was not there.", tabs: "sees" })}
      <div class="pane tl" style="margin-top:20px">
        ${bar(2, "1.4 · as it ran")}
        ${bar(3, "1.5-rc")}
        <div class="toklg">${[["s0", "Claude Code"], ["s1", "Tools"], ["s2", "Instructions, rules, task"], ["ctx", "ChloeCtx"], ["s3", "Conversation"]].map(([c, l]) => `<span><i class="${c}"></i>${l}</span>`).join("")}<span class="sp"></span><span class="mut">of a 64k window</span></div>
      </div>
      <section class="sec">${lbl("Part by part", { n: PARTS.length, right: '<span class="mut">tokens from the transcript\'s usage; Claude Code\'s own prompt is what is left</span>' })}
        <div class="pane"><table class="tbl cmp">
          <tr><th>Part</th><th class="dk">Source</th><th class="r">1.4</th><th class="r">Share</th><th class="r">1.5-rc</th><th></th></tr>
          ${PARTS.map((p) => `<tr class="${p[3] !== p[2] ? "cur" : ""}"><td><span class="sw ${p[5]}"></span>${p[0]}</td><td class="dk mut">${p[1]}</td><td class="r num">${p[2].toLocaleString("en")}</td><td class="r num">${Math.round((p[2] / t) * 100)}%</td><td class="r num ${p[3] !== p[2] ? "t-good" : "mut"}">${p[3] !== p[2] ? "+" + (p[3] - p[2]) : "same"}</td><td>${p[4] === "estimated" ? '<span class="lv est">estimated</span>' : ""}</td></tr>`).join("")}
        </table></div></section>
      <section class="sec">${lbl("The text · recall on this prompt", { link: "All parts" })}
        <div class="pane"><div class="hunk asm">
<div class="mut">&lt;memory source="chloectx" receipt="r-7b2e0c1-2"&gt;</div>
<div>  [decision · you · Nov 1] Money is integer cents everywhere; floats only at the edges.</div>
<div>  [decision · you · Nov 10] Customers win: everything a customer sees rounds half up.</div>
<div class="add">+ [decision · you · Nov 10 · pinned to cart.py] Shipping is not taxed: tax applies to the items only.</div>
<div class="mut">&lt;/memory&gt;</div></div></div></section>`,
    insp: `${lbl("Tokens nothing used", { strong: true })}
      <div class="lint">
        <div class="li warn">${ic("tri")}<div><b>playwright: 6.1k, every turn</b><span>18 tool definitions from your ~/.claude.json; no agent of shop called one in 14 days.</span></div></div>
        <div class="li">${ic("dia")}<div><b>Briefing: 1.1k, 3 of 9 notes read back</b><span>Halving it saved tokens inside the noise in run 7.</span></div></div>
      </div>
      <div>${lbl("How it is put together", { strong: true })}
        <ul class="safe">
          <li>${ic("dia")}<span><b>Measured</b>: Workbench's and ChloeCtx's parts are the text the hooks returned (their receipts), counted with the model's tokenizer.</span></li>
          <li>${ic("dia")}<span><b>From the transcript</b>: the turn's input tokens, from its usage.</span></li>
          <li>${ic("dia")}<span><b>Estimated</b>: Claude Code's own prompt is the rest. It changes with Claude Code, which every version records.</span></li>
        </ul></div>
      <div class="dock">${btn("Open turn 2 in Find out why", { pri: true, k: "W" })}${btn("Leave playwright out", { href: "#/harness/edit/tools" })}</div>`,
    mdock: btn("Open in Find out why", { pri: true }),
  };
}
