// Optimize the harness, the overview (HARNESS.md, "Screens", 1): the week's
// scorecard read live from ChloeDB, where the harness cost you (each finding
// with its evidence level), the agents by the version they ran, the
// comparison of the last two versions, and the suggestions made from the
// findings. No model is called and nothing is replayed.
import { h, useStyle, page, label, btn, bank, stateDot, icon } from "/levels/common.js";
import { tabs, lv, gauge, fmtMs, fmtTok, fmtWhen, rich, repoQ, repoName, toast, failed, loading, seg, resultLine, saveDraft } from "./parts.js";

const KIND = { storm: "Permission storm", auto_mode: "Auto mode", slow_hook: "Slow hook", unused: "Unused", redo: "Redo loop", corrected: "Corrected by you", missed_memory: "Missed memory" };
const MODE_WORDS = { default: "asks", auto: "auto", acceptEdits: "accept edits", plan: "plan", bypassPermissions: "bypass", unknown: "–" };

export function render(host, ctx) {
  host.replaceChildren(loading());
  let alive = true;
  const load = () => ctx.api(`/api/harness?${repoQ(ctx)}`).then(async (d) => {
    // Suggested rules from approvals (src/rulesuggest.rs), when this build has them.
    const rules = await ctx.api(`/api/rules/suggested?repo=${encodeURIComponent(d.repo)}`).catch(() => null);
    if (alive) draw(host, ctx, d, load, rules);
  }).catch((err) => { if (alive) host.replaceChildren(failed(err)); });
  load();
  return { unmount() { alive = false; } };
}

const KIND_WORD = { storm: "a permission storm", corrected: "your corrections", missed_memory: "missed memory", auto_mode: "auto mode's blocks", redo: "a redo loop", slow_hook: "a slow hook", unused: "unused tools" };

function draw(host, ctx, d, reload, rules) {
  const c = d.scorecard || {};
  const prev = c.prev || {};
  const fs = d.findings || [];
  const lost = fs.reduce((s, f) => s + (f.cost_ms || 0), 0);
  const name = repoName(d.repo);
  const draft = (d.drafts || [])[0];
  const title = lost > 0 ? `The harness cost you ${fmtMs(lost)} this week` : fs.length ? `${fs.length} thing${fs.length === 1 ? "" : "s"} the harness could do better` : "Nothing in the harness cost you this week";
  const sub = c.sessions
    ? `From ${c.sessions} session${c.sessions === 1 ? "" : "s"} of ${name}, counted as they ran.${fs.length ? " Most of it: " + fs.slice(0, 3).map((f) => `${KIND_WORD[f.kind] || f.kind} (${f.cost})`).join(", ") + "." : ""}${(d.suggestions || []).length ? ` ${d.suggestions.length} change${d.suggestions.length === 1 ? " is" : "s are"} ready to put in a draft.` : ""}`
    : "No sessions of this repository in the window yet: start agents, and their prompts, waits and recalls are counted here.";
  const delta = (now, before, worse = "up") => {
    if (before == null || !prev.sessions) return null;
    const dlt = Math.round((now - before) * 10) / 10;
    if (!dlt) return ["same as last week", ""];
    const bad = worse === "up" ? dlt > 0 : dlt < 0;
    return [`${dlt > 0 ? "+" : "−"}${Math.abs(dlt)} vs last week`, bad ? "bad" : "good"];
  };
  const dDone = delta(c.done || 0, prev.done, "down");
  const dPrompts = delta(c.prompts || 0, prev.prompts);
  const modes = Object.entries(c.modes || {});
  const sc = h("section", { class: "hx-sec" },
    label("This week, live", h("span", { class: "hx-num hx-mut" }, `${fmtDay(d.window.from)} – ${fmtDay(d.window.to)} · ${c.sessions || 0} sessions${(d.window.versions || []).length ? " · " + d.window.versions.join(" and ") : ""}`)),
    h("div", { class: "hx-bank6" }, bank(6,
      gauge({ label: "Done by checks", v: `${c.done || 0}/${c.ran_tests || 0}`, unit: "sessions", on: c.done, total: c.ran_tests, delta: dDone ? dDone[0] : "the last run of each test passed", dtone: dDone ? dDone[1] : "" }),
      gauge({ label: "Interventions", v: (c.per_session || 0).toFixed(1), unit: "a session", on: c.interventions, total: Math.max(c.interventions || 0, (c.sessions || 0) * 3), delta: `${c.interventions || 0} in all, ${c.prompts || 0} of them prompts` }),
      gauge({ label: "Prompts", v: String(c.prompts || 0), unit: c.top_count ? `${c.top_count} one command` : "", on: c.top_count, total: c.prompts, tone: c.prompts ? "warn" : "", delta: dPrompts ? dPrompts[0] : null, dtone: dPrompts ? dPrompts[1] : "" }),
      gauge({ label: "Wall time", v: fmtMs(c.wall_ms), unit: "median", on: Math.min(c.wall_ms || 0, 3600000), total: 3600000, delta: lost ? `${fmtMs(lost)} lost to the harness` : null, dtone: lost ? "warn" : "" }),
      gauge({ label: "Tokens", v: fmtTok(c.tokens), unit: `$${(c.cost_usd || 0).toFixed(2)}`, on: c.tokens, total: Math.max(c.tokens || 0, prev.tokens || 0) * 1.2 }),
      gauge({ label: "Memory missed", v: String(c.memory_missed || 0), unit: "turns", on: c.memory_missed, total: 24, tone: c.memory_missed ? "bad" : "", delta: c.memory_missed ? "left out, then needed" : "none recomputed" }))),
    h("p", { class: "hx-note hx-modes" },
      h("b", {}, "Permission modes: "),
      modes.length ? modes.map(([m, n]) => `${n} ${m}`).join(" · ") : "none reported",
      ". ",
      c.blocked || c.escalated ? `Auto mode blocked ${c.blocked} call${c.blocked === 1 ? "" : "s"} and handed ${c.escalated} back to you.` : "Prompts count only in sessions that ask; auto mode's blocks and escalations are counted where the events show them."));

  const findings = h("section", { class: "hx-sec" },
    label(h("span", {}, "Where the harness cost you ", h("span", { class: "hx-num" }, String(fs.length))), h("span", { class: "hx-mut" }, "Worst first")),
    fs.length ? h("div", { class: "pane" }, h("table", { class: "hx-t hx-find" },
      h("thead", {}, h("tr", {}, h("th", {}, "What"), h("th", {}, "Where"), h("th", { class: "r" }, "Sessions"), h("th", { class: "r" }, "Cost"), h("th", { class: "r" }, "Evidence"))),
      h("tbody", {}, fs.map((f) => h("tr", {},
        h("td", { "data-k": "what" }, h("span", { class: `hx-fk ${f.tone || ""}` }, KIND[f.kind] || f.kind)),
        h("td", { "data-k": "where" }, h("div", { class: "hx-fw" }, rich(f.detail)), f.agents && f.agents.length ? h("div", { class: "hx-fa" }, f.agents.join(", ")) : null),
        h("td", { class: "r hx-num", "data-k": "sessions" }, String(f.sessions || 0)),
        h("td", { class: `r hx-num hx-cost ${f.tone || ""}`, "data-k": "cost" }, f.cost),
        h("td", { class: "r", "data-k": "level" }, lv(f.level)))))))
      : h("div", { class: "pane" }, h("div", { class: "bd hx-mut" }, c.sessions ? "No storms, slow hooks, unused servers, redo loops, repeated corrections or missed memory in the window." : "Nothing counted yet.")));

  const hooks = (d.hooks || []).length ? h("section", { class: "hx-sec" },
    label("Hooks, timed", h("span", { class: "sp" }), lv("counted")),
    h("div", { class: "pane" }, h("table", { class: "hx-t" },
      h("thead", {}, h("tr", {}, h("th", {}, "Hook"), h("th", { class: "r" }, "Runs"), h("th", { class: "r" }, "p50"), h("th", { class: "r" }, "p95"), h("th", { class: "r" }, "Failed"))),
      h("tbody", {}, d.hooks.map((x) => h("tr", {}, h("td", {}, h("b", {}, x.source), " ", h("code", {}, x.event)), h("td", { class: "r hx-num" }, String(x.runs)), h("td", { class: "r hx-num" }, `${Math.round(x.p50_ms)} ms`), h("td", { class: `r hx-num ${x.p95_ms > 1000 ? "hx-warn" : ""}` }, `${x.p95_ms >= 1000 ? (x.p95_ms / 1000).toFixed(1) + " s" : Math.round(x.p95_ms) + " ms"}`), h("td", { class: "r hx-num" }, String(x.failures)))))))) : null;

  const rows = [];
  for (const g of d.by_version || []) for (const a of g.agents) rows.push([g, a]);
  const agents = h("section", { class: "hx-sec" },
    label("Agents by version", h("span", { class: "sp" }), h("a", { href: "#/harness/versions" }, "Versions")),
    rows.length ? h("div", { class: "pane" }, h("table", { class: "hx-t hx-agents" },
      h("thead", {}, h("tr", {}, h("th", {}, "Agent"), h("th", {}, "State"), h("th", {}, "Harness"), h("th", {}, "Mode"), h("th", { class: "r" }, "Started"))),
      h("tbody", {}, rows.map(([g, a]) => h("tr", { class: g.older || !g.recorded ? "hx-old" : null },
        h("td", { "data-k": "agent" }, h("a", { href: `#/harness/sees/${encodeURIComponent(a.id)}`, title: "What it saw" }, h("b", {}, a.name))),
        h("td", { "data-k": "state" }, stateDot(a.group)),
        h("td", { "data-k": "harness" }, h("code", {}, g.name), " ", h("span", { class: "hx-mut hx-num" }, g.short || ""), g.note ? h("span", { class: "hx-mut" }, ` · ${g.note}`) : null),
        h("td", { "data-k": "mode", class: "hx-mut" }, MODE_WORDS[a.mode] || a.mode),
        h("td", { class: "r hx-num", "data-k": "started" }, fmtWhen(a.created)))))))
      : h("div", { class: "pane" }, h("div", { class: "bd hx-mut" }, "No agents in the window.")),
    compareBox(d.compare));

  const sugg = d.suggestions || [];
  const insp = [
    h("div", {}, label("Suggestions", h("span", { class: "hx-num" }, String(sugg.length))),
      sugg.length ? h("div", { class: "hx-cards" }, sugg.map((s, i) => sugCard(ctx, d, s, i, reload)))
        : h("p", { class: "hx-note" }, "None from this week's findings. A suggestion is made only from a counted storm or a recomputed miss.")),
    h("div", {}, label("Auto-learn"),
      h("div", { class: "hx-gap" }, seg([["suggest", "Suggest"], ["auto", "Auto within limits"], ["nightly", "Nightly"]], "suggest", (k) => { if (k !== "suggest") toast("Not in this build: suggestions wait for you."); }, { label: "Auto-learn", disabled: ["auto", "nightly"] })),
      h("p", { class: "hx-note" }, "Suggest only: nothing changes until you keep it. A change that widens what agents may do always waits for you, by name.")),
    h("div", {}, label("Hook timing", h("span", { class: "sp" }), h("span", { class: "hx-mut" }, d.timing.on ? "on" : "off")),
      h("p", { class: "hx-note" }, d.timing.on ? "New agents time each hook into their session; slow hooks are counted from it." : d.timing.recorded ? "Some sessions were timed; new agents are not." : "Hooks are not timed. Turn it on in a draft: new agents' hooks then write how long each run took."),
      h("div", { class: "hx-gap" }, btn(d.timing.on ? "Stop timing in a draft" : "Time hooks in a draft", { sm: true, onclick: async () => {
        try {
          const base = draft || d.current;
          await saveDraft(ctx, d.repo, base.id, { hooks: { timing: !d.timing.on } });
          toast("Saved to the draft. Promote it to give it to new agents.");
          reload();
        } catch (err) { toast(err.message, "bad"); }
      } }))),
    h("div", { class: "hx-dock" },
      btn("Fix the harness", { primary: true, kbd: "⌘↵", href: "#/harness/fixes" }),
      draft ? btn(`Review ${draft.name}`, { href: "#/harness/versions" }) : btn("Edit permissions", { href: "#/harness/edit/permissions" }),
      h("div", { class: "hx-est" }, h("span", {}, "current ", h("b", { class: "hx-num" }, d.current.name)), draft ? h("span", {}, "draft ", h("b", { class: "hx-num" }, draft.name)) : h("span", {}, "no draft"))),
  ];
  const rs = rules && (rules.suggestions || rules.suggested || rules.items);
  if (Array.isArray(rs) && rs.length) insp.splice(1, 0, h("p", { class: "hx-note" }, `Rules also suggests ${rs.length} rule${rs.length === 1 ? "" : "s"} from your approvals, applied at once to new agents: `, h("a", { href: "#/more/rules" }, "Review them"), ". A rule kept there is Workbench's own here, marked Flow."));
  if ((d.missing || []).length) insp.splice(1, 0, h("p", { class: "hx-note" }, `ChloeCtx's receipts were not read for ${d.missing.length} session${d.missing.length === 1 ? "" : "s"}: missed memory counts only the rest.`));

  host.replaceChildren(page({ kicker: `Optimize the harness · ${name} · ${d.current.name}`, title, sub, inspector: insp, cls: "hx-page" },
    tabs("overview"), sc, findings, hooks, agents));
}

function fmtDay(t) { return new Date(t).toLocaleDateString("en", { month: "short", day: "numeric" }); }

function compareBox(cmp) {
  if (!cmp) return null;
  if (cmp.result === "none") return h("div", { class: "pane hx-cmp" }, resultLine("none", cmp.note));
  return h("div", { class: "pane hx-cmp" },
    h("div", { class: "hd" }, label(`${cmp.a.name} against ${cmp.b.name}`, h("span", { class: "sp" }), lv(cmp.level))),
    resultLine(cmp.done.result, cmp.sentences[0]),
    resultLine(cmp.prompts.result, cmp.sentences[1]));
}

function sugCard(ctx, d, s, i, reload) {
  const put = async () => {
    try {
      const draft = (d.drafts || [])[0];
      const base = draft || d.current;
      if (s.part === "permissions") {
        const p = await ctx.api(`/api/harness/permissions?repo=${encodeURIComponent(d.repo)}&version=${encodeURIComponent(base.id)}`);
        const e = { allow: [], ask: [], deny: [], remove: [], ...p.edits };
        for (const r of s.edit.permissions.allow || []) if (!e.allow.includes(r)) e.allow.push(r);
        await saveDraft(ctx, d.repo, base.id, { permissions: e });
        location.hash = "#/harness/edit/permissions";
      } else {
        const m = await ctx.api(`/api/harness/memory?repo=${encodeURIComponent(d.repo)}&version=${encodeURIComponent(base.id)}`);
        const e = { ...m.edits, pins: [...(m.edits.pins || [])] };
        for (const pin of s.edit.memory.pins || []) if (!e.pins.some((x) => x.record === pin.record)) e.pins.push(pin);
        await saveDraft(ctx, d.repo, base.id, { memory: e });
        location.hash = "#/harness/edit/memory";
      }
    } catch (err) {
      if (err.status === 409) { toast("Already in the draft."); reload(); } else toast(err.message, "bad");
    }
  };
  return h("div", { class: "hx-card" },
    h("div", { class: "hx-ch" }, h("span", { class: "lbl" }, s.part), s.widens ? h("span", { class: "hx-flag" }, icon("warn"), "widens") : null, h("span", { class: "sp" }), lv(s.level)),
    h("div", { class: "hx-ask" }, rich(s.title)),
    h("div", { class: "hx-diff" }, s.diff.map(([g, t]) => h("div", { class: g === "+" ? "a" : "d" }, `${g} ${t}`))),
    h("p", { class: "hx-what" }, rich(s.why)),
    h("div", { class: "hx-imp hx-num" }, s.impact),
    s.alt ? h("p", { class: "hx-what" }, s.alt) : null,
    h("button", { type: "button", class: "hx-opt", onclick: put }, h("kbd", { class: "kc" }, String(i + 1)), "Put it in a draft"));
}

useStyle("harness-overview", `
.hx-page .gp-main{max-width:none}
.hx-bank6 .bank{--cols:6}
.hx-modes b{color:var(--text);font-weight:var(--w-mid)}
.hx-t{width:100%;border-collapse:collapse;font-size:var(--fs-body)}
.hx-t th{text-align:left;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2);padding:10px var(--s-4);border-bottom:var(--hair) solid var(--line);white-space:nowrap}
.hx-t td{padding:10px var(--s-4);border-bottom:var(--hair) solid var(--line);vertical-align:top;min-height:44px}
.hx-t tr:last-child td{border-bottom:0}
.hx-t .r{text-align:right}
.hx-t a{color:inherit;text-decoration:none}.hx-t a:hover b{text-decoration:underline}
.hx-find td{padding-top:12px;padding-bottom:12px}
.hx-find td:first-child{width:150px}
.hx-fk{font:var(--w-mid) var(--fs-xs)/16px var(--mono);letter-spacing:.06em;text-transform:uppercase;white-space:nowrap;color:var(--text-2)}
.hx-fk.warn,.hx-cost.warn{color:var(--warn-ink)}.hx-fk.bad,.hx-cost.bad{color:var(--crit-ink)}
.hx-fw{max-width:620px;overflow-wrap:anywhere}
.hx-fa{color:var(--text-2);font-size:var(--fs-sm);margin-top:2px}
.hx-warn{color:var(--warn-ink)}
.hx-old td{color:var(--text-2)}
.hx-cmp{margin-top:var(--s-3)}
.hx-cards{display:flex;flex-direction:column;gap:var(--s-4);margin-top:var(--s-3)}
.hx-card{background:var(--surface);border:var(--hair) solid var(--line);border-radius:var(--r);overflow:hidden}
.hx-ch{display:flex;align-items:center;gap:var(--s-2);padding:var(--s-3) var(--s-4) 0}
.hx-ch .sp{flex:1}
.hx-ask{padding:6px var(--s-4) 0;font-weight:var(--w-title);font-size:var(--fs-md);line-height:var(--lh-md)}
.hx-diff{margin:var(--s-3) var(--s-4) 0;padding:6px 10px;border-radius:var(--r-sm);background:var(--well);font:var(--w-text) var(--fs-sm)/var(--lh-sm) var(--mono);overflow-wrap:anywhere;white-space:pre-wrap}
.hx-diff .a{color:var(--good-ink)}.hx-diff .d{color:var(--crit-ink)}
.hx-what{margin:0;padding:var(--s-2) var(--s-4) 0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.hx-imp{padding:6px var(--s-4) 0;font-size:var(--fs-sm);font-weight:var(--w-mid)}
.hx-opt{display:flex;align-items:center;gap:10px;width:100%;min-height:44px;margin-top:var(--s-3);padding:var(--s-2) var(--s-4);border:0;border-top:var(--hair) solid var(--line);background:var(--accent-soft);color:var(--accent-text);font:var(--w-mid) var(--fs-body) var(--sans);text-align:left;cursor:pointer}
.hx-opt:hover{background:var(--surface-2)}
.hx-gap{margin-top:var(--s-3)}
.hx-dock{margin-top:auto;display:flex;flex-direction:column;gap:var(--s-2);padding-top:var(--s-4);border-top:var(--hair) solid var(--line)}
.hx-dock .btn{justify-content:center;width:100%}
.hx-est{display:flex;justify-content:space-between;gap:var(--s-3);font:var(--w-text) var(--fs-xs)/16px var(--mono);color:var(--text-2)}
.hx-est b{color:var(--text);font-weight:var(--w-mid)}
@media (max-width:1180px){.hx-bank6 .bank{--cols:3}}
@media (max-width:640px){
  .hx-bank6 .bank{grid-template-columns:repeat(2,minmax(0,1fr))}
  .hx-t thead{display:none}
  .hx-t,.hx-t tbody,.hx-t tr{display:block}
  .hx-t tr{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:2px var(--s-3);padding:var(--s-3) var(--s-4);border-bottom:var(--hair) solid var(--line)}
  .hx-t tr:last-child{border-bottom:0}
  .hx-t td{display:block;padding:0;border:0;text-align:left;min-height:0}
  .hx-t td.r{text-align:right}
  .hx-find td[data-k=where]{grid-column:1 / -1;order:3}
  .hx-find td[data-k=sessions]{display:none}
  .hx-find td[data-k=level]{order:4;grid-column:1 / -1;text-align:left}
  .hx-agents td[data-k=harness]{grid-column:1 / -1}
  .hx-agents td[data-k=mode]{display:none}
}
`);
