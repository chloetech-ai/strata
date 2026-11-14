// Optimize the harness, versions (HARNESS.md, "Versions, promote and roll
// back"): the repository's versions with the agents that ran each, the
// draft's diff against the current one by part with its counted impact,
// Promote (a widening confirmed by name) and Roll back. Promoting changes
// only what Workbench gives new agents; running agents keep their version.
import { h, useStyle, page, label, btn, icon } from "/levels/common.js";
import { tabs, lv, fmtWhen, rich, repoQ, repoName, toast, failed, loading, promote, resultLine } from "./parts.js";

export function render(host, ctx) {
  host.replaceChildren(loading("Reading the versions…"));
  const S = { alive: true };
  const load = async () => {
    try {
      const d = await ctx.api(`/api/harness/versions?${repoQ(ctx)}`);
      const draft = d.versions.find((v) => v.state === "draft");
      let imp = null;
      if (draft) imp = await ctx.api("/api/harness/impact", { method: "POST", body: { repo: d.repo, edits: draft.edits.permissions } }).catch(() => null);
      if (S.alive) draw(host, ctx, d, draft, imp, load);
    } catch (err) { if (S.alive) host.replaceChildren(failed(err)); }
  };
  load();
  return { unmount() { S.alive = false; } };
}

function diffParts(a, b) {
  const out = [];
  const pa = a.permissions || {}, pb = b.permissions || {};
  const lines = [];
  for (const k of ["allow", "ask", "deny"]) {
    for (const r of pb[k] || []) if (!(pa[k] || []).includes(r)) lines.push(["+", `${k.padEnd(5)}  ${r}`]);
    for (const r of pa[k] || []) if (!(pb[k] || []).includes(r)) lines.push(["−", `${k.padEnd(5)}  ${r}`]);
  }
  for (const r of pb.remove || []) if (!(pa.remove || []).includes(r)) lines.push(["−", `Workbench's  ${r}`]);
  for (const r of pa.remove || []) if (!(pb.remove || []).includes(r)) lines.push(["+", `Workbench's  ${r}`]);
  if (lines.length) out.push(["Permissions", lines]);
  const ma = a.memory || {}, mb = b.memory || {};
  const ml = [];
  for (const k of ["gate_hi", "gate_lo", "knowledge_min_cos", "budget_chars", "briefing_chars"]) if ((ma[k] ?? null) !== (mb[k] ?? null)) ml.push(["~", `${k}  ${ma[k] ?? "ChloeCtx's"} → ${mb[k] ?? "ChloeCtx's"}`]);
  const pins = (m) => (m.pins || []).map((p) => `${p.file} → “${p.text}”`);
  for (const p of pins(mb)) if (!pins(ma).includes(p)) ml.push(["+", `pin  ${p}`]);
  for (const p of pins(ma)) if (!pins(mb).includes(p)) ml.push(["−", `pin  ${p}`]);
  if (ml.length) out.push(["Memory", ml]);
  if (!!(a.hooks || {}).timing !== !!(b.hooks || {}).timing) out.push(["Hooks", [["~", `timing  ${(a.hooks || {}).timing ? "on" : "off"} → ${(b.hooks || {}).timing ? "on" : "off"}`]]]);
  return out;
}

function draw(host, ctx, d, draft, imp, reload) {
  const cur = d.versions.find((v) => v.state === "current") || d.current;
  const parts = draft ? diffParts(cur.edits || {}, draft.edits || {}) : [];
  const widens = imp && imp.widens && imp.widens.widens;
  const doPromote = async (v) => {
    try {
      const r = await promote(ctx, v.id);
      if (!r) { toast("Not promoted: the widening was not confirmed."); return; }
      toast(`${r.version.name} is current: new agents get it.`);
      reload();
    } catch (err) { toast(err.message, "bad"); }
  };
  const doRollback = async () => {
    try {
      const r = await ctx.api(`/api/harness/versions/${encodeURIComponent(cur.id)}/rollback`, { method: "POST", body: { reason: "from the versions page" } });
      toast(`Rolled back: ${r.version.name} is current again.`);
      reload();
    } catch (err) { toast(err.message, "bad"); }
  };
  const canRoll = (d.history || []).filter((x) => x !== cur.id).length > 0;
  const title = draft ? `${draft.name} is ready to replace ${cur.name}` : `${cur.name} is what new agents get`;
  const history = h("section", { class: "hx-sec" },
    label(h("span", {}, "History ", h("span", { class: "hx-num" }, String(d.versions.length))), h("span", { class: "hx-mut hx-rule-word" }, "agents that ran each")),
    h("div", { class: "pane" }, h("table", { class: "hx-t hx-vers" },
      h("thead", {}, h("tr", {}, h("th", {}), h("th", {}, "Version"), h("th", {}, "Change"), h("th", { class: "r" }, "Agents"), h("th", { class: "r" }, "Made"))),
      h("tbody", {}, d.versions.map((v) => h("tr", { class: v.state === "draft" ? "hx-chg" : null },
        h("td", { "data-k": "dot" }, h("span", { class: `hx-gd ${v.state}` })),
        h("td", { "data-k": "v" }, h("b", { class: "hx-num" }, v.name), " ", h("span", { class: "hx-mut hx-num" }, v.short), " ", h("span", { class: `hx-vs ${v.state}` }, v.state), v.widens ? h("span", { class: "hx-flag" }, icon("warn"), "widened, approved") : null),
        h("td", { "data-k": "note" }, v.note || summary(v.edits), h("div", { class: "hx-fa" }, `${v.made_by === "workbench" ? "Workbench" : "you"}${v.runs.length ? ` · ${v.runs.length} run version${v.runs.length === 1 ? "" : "s"} (${[...new Set(v.runs.map((r) => r.claude).filter(Boolean))].join(", ") || "Claude Code unknown"})` : ""}`)),
        h("td", { class: "r hx-num", "data-k": "agents" }, String(v.agents)),
        h("td", { class: "r hx-num hx-mut", "data-k": "made" }, fmtWhen(v.promoted || v.created))))))));
  const diff = draft ? h("section", { class: "hx-sec" },
    label(`${cur.name} → ${draft.name}`, h("span", { class: "hx-num" }, `${parts.length} part${parts.length === 1 ? "" : "s"}`)),
    h("div", { class: "pane" }, parts.length ? parts.map(([name, lines]) => h("div", { class: "hx-dpart" },
      h("div", { class: "hx-dph" }, h("b", {}, name), name === "Permissions" && widens ? h("span", { class: "hx-flag" }, icon("warn"), "widens") : null, h("span", { class: "sp" }),
        name === "Permissions" && imp ? [lv("counted"), h("span", { class: "hx-mut hx-num" }, ` ${imp.saved ? `−${imp.saved}` : imp.added ? `+${imp.added}` : "no change in"} prompts in 7 days · ${imp.denies_changed} denies changed`)] : name === "Memory" ? lv("recomputed") : null),
      h("div", { class: "hx-diff" }, lines.map(([g, t]) => h("div", { class: g === "+" ? "a" : g === "−" ? "d" : "" }, `${g} ${t}`)))))
      : h("div", { class: "bd hx-mut" }, "The draft is the same as the current version."),
      imp && imp.result === "no_difference" && parts.some(([n]) => n === "Permissions") ? resultLine("no_difference", `The permissions decide each of the ${imp.cases} calls of the last 7 days as ${cur.name} does.`) : null)) : null;
  const insp = [
    draft ? h("div", {}, label(`Promote ${draft.name}`),
      h("dl", { class: "kv" },
        h("dt", {}, "New agents"), h("dd", {}, `start with ${draft.name.replace(/-draft$/, "")} from now on`),
        h("dt", {}, "Running"), h("dd", {}, `keep ${cur.name} until they restart`),
        h("dt", {}, "Approval"), h("dd", {}, widens ? [h("span", { class: "hx-warn" }, "It widens: "), (imp.widens.patterns || []).map((p, i) => [i ? ", " : "", h("code", {}, p)]), ". You confirm each by name."] : "nothing widens"),
        h("dt", {}, "Back"), h("dd", {}, `Roll back makes ${cur.name} current again; nothing is deleted`),
        h("dt", {}, "Yours"), h("dd", {}, "~/.claude and the repository's files are never written"))) : h("div", {}, label("No draft"), h("p", { class: "hx-note" }, "Edit permissions or memory, or put a suggestion in a draft, and it shows here.")),
    canRoll ? h("div", {}, label("Roll back"), h("p", { class: "hx-note" }, `Makes the version before ${cur.name} current again for new agents.`), h("div", { class: "hx-gap" }, btn(`Roll back ${cur.name}`, { onclick: doRollback }))) : null,
    h("div", { class: "hx-dock" },
      draft ? btn(`Promote ${draft.name.replace(/-draft$/, "")} to ${repoName(d.repo)}`, { primary: true, kbd: "⌘↵", onclick: () => doPromote(draft) }) : btn("Edit permissions", { primary: true, href: "#/harness/edit/permissions" }),
      draft ? btn("Edit the draft", { href: "#/harness/edit/permissions" }) : null),
  ];
  host.replaceChildren(page({ kicker: `Optimize the harness · versions · ${repoName(d.repo)}`, title, sub: "A version is every part Workbench gives an agent, by its hash. Each agent records the one it started with; promoting changes what new agents start with, and rolling back is one step.", inspector: insp, cls: "hx-page" },
    tabs("versions"), diff, history));
}

function summary(e) {
  if (!e) return "";
  const p = e.permissions || {};
  const bits = [];
  const n = (p.allow || []).length + (p.ask || []).length + (p.deny || []).length;
  if (n) bits.push(`${n} rule${n === 1 ? "" : "s"} added`);
  if ((p.remove || []).length) bits.push(`${p.remove.length} of Workbench's taken out`);
  if ((e.memory || {}).pins && e.memory.pins.length) bits.push(`${e.memory.pins.length} pin${e.memory.pins.length === 1 ? "" : "s"}`);
  if (["gate_hi", "gate_lo", "knowledge_min_cos", "budget_chars", "briefing_chars"].some((k) => (e.memory || {})[k] != null)) bits.push("recall settings");
  if ((e.hooks || {}).timing) bits.push("hooks timed");
  return bits.length ? rich(bits.join(" · ")) : "Workbench's own";
}

useStyle("harness-versions", `
.hx-gd{display:inline-block;width:10px;height:10px;border-radius:50%;border:1.5px solid var(--text-3)}
.hx-gd.current{background:var(--good);border-color:var(--good)}.hx-gd.draft{border-color:var(--accent);background:var(--accent-soft)}
.hx-vs{font:var(--w-mid) var(--fs-xs)/16px var(--mono);letter-spacing:.06em;text-transform:uppercase;color:var(--text-2);margin-left:4px}
.hx-vs.current{color:var(--good-ink)}.hx-vs.draft{color:var(--accent-text)}.hx-vs.dropped{color:var(--text-3)}
.hx-vers td[data-k=note]{overflow-wrap:anywhere}
.hx-chg td{background:var(--accent-soft)}
.hx-dpart{padding:var(--s-3) var(--s-4) var(--s-4);border-bottom:var(--hair) solid var(--line)}
.hx-dpart:last-child{border-bottom:0}
.hx-dph{display:flex;align-items:center;gap:var(--s-2);flex-wrap:wrap}.hx-dph .sp{flex:1}
.hx-dpart .hx-diff{margin:var(--s-2) 0 0}
@media (max-width:640px){
  .hx-vers td[data-k=dot]{display:none}
  .hx-vers td[data-k=note]{grid-column:1 / -1}
  .hx-vers td[data-k=made]{display:none}
  .hx-vers td[data-k=agents]{grid-row:1;grid-column:2;color:var(--text-2)}
  .hx-vers td[data-k=agents]::after{content:" agents"}
  .hx-chg{background:var(--accent-soft)}.hx-chg td{background:none}
}
`);
