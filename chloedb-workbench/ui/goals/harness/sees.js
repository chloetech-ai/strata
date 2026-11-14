// Optimize the harness, what the model sees (HARNESS.md, "What the model
// sees"): an agent's context at one turn, part by part. The turn (its
// prompt, its size from the transcript's usage, what ChloeCtx gave and left
// out) is Debug's, GET /api/agents/:id/turns/:n/context (src/turns.rs);
// GET /api/harness/sees adds what Workbench injected, what Claude Code
// loads by itself (estimated), the harness version and whether the raw
// request bodies are exported. Workbench never turns that export on.
import { h, useStyle, page, label, btn, stateDot } from "/levels/common.js";
import { tabs, lv, fmtTok, fmtWhen, repoQ, failed, loading } from "./parts.js";

const enc = encodeURIComponent;

export function render(host, ctx, path) {
  host.replaceChildren(loading("Reading the turn…"));
  const S = { alive: true };
  const agent = decodeURIComponent((path.split("/")[3] || "").trim());
  const turnQ = Number(new URLSearchParams((location.hash.split("?")[1]) || "").get("turn")) || null;
  (async () => {
    try {
      const ov = await ctx.api(`/api/harness?${repoQ(ctx)}`);
      const agents = [];
      for (const g of ov.by_version || []) for (const a of g.agents) agents.push({ ...a, version: g.name });
      agents.sort((a, b) => (b.created_ms || 0) - (a.created_ms || 0));
      const id = agent || (agents[0] && agents[0].id);
      if (!id) { host.replaceChildren(page({ kicker: "Optimize the harness", title: "What the model sees", sub: "No agents in the window yet.", cls: "hx-page" }, tabs("sees"))); return; }
      let n = turnQ;
      if (!n) {
        const t = await ctx.api(`/api/agents/${enc(id)}/turns`).catch(() => null);
        n = t && t.turns && t.turns.length ? t.turns[t.turns.length - 1].n : 1;
      }
      const turn = await ctx.api(`/api/agents/${enc(id)}/turns/${n}/context`).catch((err) => ({ error: err.message, turn: n, turns: 0 }));
      const mine = await ctx.api(`/api/harness/sees?agent=${enc(id)}${turn.ended_ms ? `&until_ms=${turn.ended_ms}` : ""}`);
      if (S.alive) draw(host, turn, mine, agents);
    } catch (err) { if (S.alive) host.replaceChildren(failed(err)); }
  })();
  return { unmount() { S.alive = false; } };
}

const TONE = { injected: "inj", estimated: "est", counted: "cnt" };
const est = (t) => Math.ceil(String(t || "").length / 4);
const share = (n, of) => { const p = (n / of) * 100; return n > 0 && p < 1 ? "<1%" : `${Math.round(p)}%`; };

function draw(host, turn, mine, agents) {
  const parts = [];
  if (turn.prompt && turn.prompt.text) parts.push({ part: "The prompt", source: turn.prompt.by === "you" ? "your words" : `from ${turn.prompt.by || "the hook's input"}`, tokens: est(turn.prompt.text), level: "injected", text: turn.prompt.text });
  parts.push(...mine.parts);
  const mem = turn.memory || {};
  if ((mem.injected || []).length) parts.push({ part: "ChloeCtx's records", source: `${mem.injected.length} given · ${(mem.missed || []).length} missed · Debug's receipt read`, tokens: (turn.context && turn.context.memory_tokens) || est(mem.injected.map((r) => r.text).join("\n")), level: "injected", text: mem.injected.map((r) => `- [${r.kind}] ${r.text}`).join("\n") });
  const total = turn.context && turn.context.tokens;
  const known = parts.reduce((s, p) => s + (p.tokens || 0), 0);
  parts.push({ part: "Claude Code's system prompt, tool definitions and the conversation", source: `what is left of the turn's input; tools from Claude Code${mine.tools.servers.length ? " and " + mine.tools.servers.join(", ") : ""}`, tokens: total != null ? Math.max(0, total - known) : null, level: "estimated" });
  const injected = parts.filter((p) => p.level === "injected").reduce((s, p) => s + (p.tokens || 0), 0);
  const go = (n) => { location.hash = `#/harness/sees/${enc(mine.agent.id)}?turn=${n}`; };
  const bar = h("div", { class: "hx-tok", role: "img", "aria-label": `${fmtTok(total || known)} tokens: ${fmtTok(injected)} injected, the rest estimated` },
    parts.filter((p) => p.tokens > 0).map((p, i) => h("i", { class: `${TONE[p.level] || ""} s${i % 3}`, style: { flex: String(p.tokens) }, title: `${p.part}: ${p.tokens} tokens (${p.level})` })));
  const picker = h("select", { class: "field hx-agentpick", "aria-label": "Agent" }, agents.map((a) => h("option", { value: a.id, selected: a.id === mine.agent.id ? true : null }, `${a.name} · ${a.version}`)));
  picker.addEventListener("change", () => { location.hash = `#/harness/sees/${enc(picker.value)}`; });
  const me = agents.find((a) => a.id === mine.agent.id);
  const n = turn.turn || 1, of = turn.turns || 0;
  const insp = [
    h("div", {}, label("Agent"), h("div", { class: "hx-gap" }, picker),
      h("p", { class: "hx-note" }, me ? stateDot(me.group) : null, " · harness ", h("code", {}, mine.agent.version || "–"), mine.agent.mode ? ` · ${mine.agent.mode} mode` : " · mode not reported")),
    h("div", {}, label("Turn", h("span", { class: "sp" }), h("span", { class: "hx-num" }, of ? `${n} of ${of}` : String(n))),
      h("div", { class: "hx-acts hx-gap" }, btn("Earlier", { sm: true, disabled: n <= 1, onclick: () => go(n - 1) }), btn("Later", { sm: true, disabled: !of || n >= of, onclick: () => go(n + 1) }),
        h("a", { class: "btn sm", href: `#/debug/${enc(mine.agent.id)}/${n}` }, "Open in Debug"))),
    h("div", {}, label("Claude Code's raw request"), h("p", { class: "hx-note" }, mine.raw_bodies.note)),
    h("div", {}, label("How it is known"), h("p", { class: "hx-note" }, mine.note), h("p", { class: "hx-note" }, mine.tools.note)),
  ];
  const sub = turn.error ? `Debug could not read this turn: ${turn.error}`
    : total ? `${fmtTok(total)} tokens before it wrote a word, counted from the transcript${turn.context.pct != null ? ` (${turn.context.pct}% of its window)` : ""}. ${fmtTok(injected)} of them Workbench and ChloeCtx put in and recorded; the rest Claude Code assembled, and is estimated.`
      : "The transcript has no usage for this turn: only what Workbench and ChloeCtx put in is shown.";
  host.replaceChildren(page({ kicker: `Optimize the harness · ${mine.agent.name} · turn ${n} · ${mine.agent.version || ""}`, title: `What ${mine.agent.name} saw at turn ${n}`, sub, inspector: insp, cls: "hx-page" },
    tabs("sees"),
    h("div", { class: "pane hx-tl" },
      h("div", { class: "hx-tokrow" }, h("span", { class: "lbl" }, `as it ran · ${fmtWhen(turn.started_ms)}`), bar, h("span", { class: "hx-num hx-tokt" }, total ? `${fmtTok(total)} counted` : "–")),
      h("div", { class: "hx-toklg" }, h("span", {}, h("i", { class: "inj" }), "Injected, recorded"), h("span", {}, h("i", { class: "est" }), "Estimated"))),
    h("section", { class: "hx-sec" }, label("Part by part", h("span", { class: "hx-num" }, String(parts.length))),
      h("div", { class: "pane" }, h("table", { class: "hx-t hx-parts" },
        h("thead", {}, h("tr", {}, h("th", {}, "Part"), h("th", {}, "Source"), h("th", { class: "r" }, "Tokens"), h("th", { class: "r" }, "Share"), h("th", { class: "r" }, "Evidence"))),
        h("tbody", {}, parts.map((p) => h("tr", {},
          h("td", { "data-k": "part" }, h("span", { class: `hx-sw2 ${TONE[p.level] || ""}` }), p.part),
          h("td", { "data-k": "src", class: "hx-mut" }, p.source),
          h("td", { class: "r hx-num", "data-k": "tok" }, p.tokens == null ? "–" : fmtTok(p.tokens)),
          h("td", { class: "r hx-num hx-mut", "data-k": "share" }, total && p.tokens != null ? share(p.tokens, total) : "–"),
          h("td", { class: "r", "data-k": "lv" }, lv(p.level)))))))),
    h("section", { class: "hx-sec" }, label("The text put in"),
      parts.filter((p) => p.text).map((p) => h("details", { class: "pane hx-txt", open: p.part === "ChloeCtx's records" ? true : null }, h("summary", {}, label(p.part, h("span", { class: "sp" }), lv(p.level))), h("pre", {}, p.text))))));
}

useStyle("harness-sees", `
.hx-tl{padding:var(--s-4)}
.hx-tokrow{display:grid;grid-template-columns:150px minmax(0,1fr) auto;gap:var(--s-3);align-items:center}
.hx-tok{display:flex;gap:2px;height:22px;border-radius:var(--r-xs);overflow:hidden;background:var(--surface-3)}
.hx-tok i{display:block;min-width:3px;background:var(--line-2)}
.hx-tok i.inj{background:var(--accent)}.hx-tok i.inj.s1{background:var(--ctx)}.hx-tok i.inj.s2{background:var(--accent-line)}
.hx-tok i.est{background:repeating-linear-gradient(135deg,var(--line-2) 0 4px,var(--surface-3) 4px 8px)}
.hx-tokt{font-size:var(--fs-sm)}
.hx-toklg{display:flex;gap:var(--s-4);flex-wrap:wrap;margin-top:var(--s-3);font:var(--w-text) var(--fs-xs)/16px var(--mono);color:var(--text-2)}
.hx-toklg span{display:inline-flex;align-items:center;gap:6px}
.hx-toklg i,.hx-sw2{display:inline-block;width:10px;height:10px;border-radius:2px;background:var(--line-2);flex:none}
.hx-toklg i.inj,.hx-sw2.inj{background:var(--accent)}
.hx-toklg i.est,.hx-sw2.est{background:repeating-linear-gradient(135deg,var(--line-2) 0 3px,var(--surface-3) 3px 6px)}
.hx-sw2{margin-right:8px;vertical-align:-1px}
.hx-parts td[data-k=src]{overflow-wrap:anywhere;max-width:420px}
.hx-txt{margin-bottom:var(--s-3)}
.hx-txt summary{list-style:none;cursor:pointer;padding:var(--s-3) var(--s-4);min-height:44px}
.hx-txt summary::-webkit-details-marker{display:none}
.hx-txt pre{margin:0;padding:var(--s-3) var(--s-4);border-top:var(--hair) solid var(--line);background:var(--well);font:var(--w-text) var(--fs-sm)/var(--lh-sm) var(--mono);white-space:pre-wrap;overflow-wrap:anywhere;max-height:360px;overflow:auto}
.hx-agentpick{font-size:var(--fs-body)}
@media (max-width:640px){
  .hx-tokrow{grid-template-columns:minmax(0,1fr)}
  .hx-parts td[data-k=part]{grid-column:1;grid-row:1}
  .hx-parts td[data-k=lv]{grid-column:2;grid-row:1;text-align:right}
  .hx-parts td[data-k=src]{grid-column:1;grid-row:2}
  .hx-parts td[data-k=tok]{grid-column:2;grid-row:2}
  .hx-parts td[data-k=share]{display:none}
}
`);
