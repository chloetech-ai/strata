// Optimize the harness, Fix the harness (HARNESS.md, "Propose"; src/harness/
// fixes.rs): every problem the agents caused this week, joined into one ranked
// list of changes to the harness. The best fix that does not widen what agents
// may do is recommended and the only one ticked. Applying puts the ticked ones
// in a draft (never in the current version), and sending an agent back is a
// separate box: a fix is about every agent to come, a note about one agent's work.
import { h, useStyle, page, label, btn, icon } from "/levels/common.js";
import { tabs, lv, fmtMs, rich, repoQ, repoName, toast, failed, loading } from "./parts.js";

export function render(host, ctx) {
  host.replaceChildren(loading("Reading this week's problems…"));
  let alive = true;
  const load = () => ctx.api(`/api/harness/fixes?${repoQ(ctx)}`)
    .then((d) => { if (alive) draw(host, ctx, d, load); })
    .catch((err) => { if (alive) host.replaceChildren(failed(err)); });
  load();
  return { unmount() { alive = false; } };
}

const KIND_WORD = { storm: "permission storm", missed_memory: "missed memory", corrected: "your corrections", redo: "redo loop", slow_hook: "slow hook", unused: "unused tool", auto_mode: "auto mode" };

function draw(host, ctx, d, reload) {
  const name = repoName(d.repo);
  const fixes = d.fixes || [];
  const ideas = d.ideas || [];
  const sum = d.summary || {};
  const ticked = new Set(fixes.filter((f) => f.recommended).map((f) => f.id));
  const boxes = new Map();
  const applyBtn = btn("Put the ticked fixes in a draft", { primary: true, kbd: "⌘↵", onclick: () => apply() });
  const tally = h("div", { class: "hx-est hx-num" });
  const cards = h("div", { class: "hx-cards" });

  const title = fixes.length
    ? `${fixes.length} change${fixes.length === 1 ? "" : "s"} to the harness would answer ${sum.answered} problem${sum.answered === 1 ? "" : "s"}`
    : sum.problems ? "No change to the harness fixes this week's problems yet" : "Nothing in the harness to fix this week";
  const sub = fixes.length
    ? `Best first, by the time each would have saved. One is ticked: ${fixes.find((f) => f.recommended) ? "the best that does not widen what agents may do" : "none, because each widens what agents may do"}. Nothing changes until you put them in a draft and promote it.`
    : "A fix is made from a counted permission storm or a recomputed miss. Start agents and work for a week, and what cost you time is listed here.";

  function sync() {
    for (const [id, b] of boxes) b.checked = ticked.has(id);
    const on = fixes.filter((f) => ticked.has(f.id));
    const n = (k) => on.reduce((s, f) => s + (f.numbers ? f.numbers[k] || 0 : 0), 0);
    applyBtn.disabled = !on.length;
    const parts = [];
    if (n("prompts")) parts.push(`−${n("prompts")} prompts`);
    if (n("records")) parts.push(`+${n("records")} records`);
    if (n("misses")) parts.push(`${n("misses")} miss${n("misses") === 1 ? "" : "es"} fixed`);
    if (n("ms")) parts.push(`${fmtMs(n("ms"))} back`);
    tally.replaceChildren(h("span", {}, on.length ? `${on.length} ticked` : "none ticked"), h("span", {}, parts.join(" · ") || "–"));
  }

  async function apply() {
    try {
      const r = await ctx.api("/api/harness/fixes/apply", { method: "POST", body: { repo: d.repo, ids: [...ticked] } });
      toast(`Saved to ${r.version ? r.version.name : "the draft"}${r.widens ? ", and it widens what agents may do: promoting asks you to confirm" : ""}. Promote it to give it to new agents.`);
      location.hash = "#/harness/versions";
    } catch (err) {
      if (err.status === 409 && err.code === "unchanged") { toast("Already in a version."); reload(); } else toast(err.message, "bad");
    }
  }

  fixes.forEach((f, i) => {
    const box = h("input", { type: "checkbox", id: `fx-${f.id}`, "aria-label": `Tick: ${f.title}` });
    box.addEventListener("change", () => { box.checked ? ticked.add(f.id) : ticked.delete(f.id); sync(); });
    boxes.set(f.id, box);
    cards.append(h("div", { class: `hx-card hx-fx${f.recommended ? " rec" : ""}`, "data-fix": f.id },
      h("div", { class: "hx-ch" },
        h("label", { class: "hx-tick", for: `fx-${f.id}` }, box, h("span", { class: "lbl" }, `${i + 1} · ${f.part}`)),
        f.recommended ? h("span", { class: "hx-rec" }, icon("check"), "recommended") : null,
        f.widens ? h("span", { class: "hx-flag" }, icon("warn"), "widens") : null,
        h("span", { class: "sp" }), lv(f.level)),
      h("div", { class: "hx-ask" }, rich(f.title)),
      h("div", { class: "hx-diff" }, f.diff.map(([g, t]) => h("div", { class: g === "+" ? "a" : "d" }, `${g} ${t}`))),
      h("div", { class: "hx-imp hx-num" }, f.impact, f.cost ? ` · ${f.cost}` : ""),
      h("div", { class: "hx-ans" },
        h("div", { class: "hx-ansh" }, `Answers ${f.problems} problem${f.problems === 1 ? "" : "s"}`),
        h("ul", {}, (f.answers || []).map((a) => h("li", {},
          h("span", { class: "hx-fk" }, KIND_WORD[a.kind] || a.kind), " ",
          a.href ? h("a", { href: a.href }, a.title) : a.title,
          a.detail ? h("span", { class: "hx-mut" }, ` — ${a.detail}`) : null)))),
      f.alt ? h("p", { class: "hx-what" }, f.alt) : null));
  });

  const ideaList = ideas.length ? h("section", { class: "hx-sec" },
    label("Problems no editor can fix yet", h("span", { class: "hx-num hx-mut" }, String(ideas.length))),
    h("p", { class: "hx-note" }, "Each is counted, and its fix is clear, but this build has no editor for that part of the harness. They are listed so the total is honest."),
    h("table", { class: "hx-t hx-ideas" }, h("tbody", {}, ideas.map((x) => h("tr", {},
      h("td", { "data-k": "what" }, h("div", {}, h("b", {}, x.title)), h("div", { class: "hx-fa" }, rich(x.problem))),
      h("td", { class: "r hx-num", "data-k": "cost" }, x.cost || ""),
      h("td", { "data-k": "where", class: "hx-mut" }, x.link ? h("a", { href: x.link }, "Open") : x.needs ? `waits for ${x.needs}` : "")))))) : null;

  const back = (d.agents || []).length ? h("div", {}, label("Send an agent back"),
    h("p", { class: "hx-note" }, "This is about one agent's work and is separate from the fixes: ticking a fix sends nobody anything. Open the turn that went wrong, where Send back has a note ready."),
    h("div", { class: "hx-gap hx-links" }, d.agents.map((a) => h("a", { class: "btn sm", href: `#/debug/${encodeURIComponent(a.id)}${a.turn ? "/" + a.turn : ""}` }, `${a.name}${a.turn ? ", turn " + a.turn : ""}`)))) : null;

  const draft = d.draft;
  const insp = [
    h("div", {}, label("This week", h("span", { class: "hx-num hx-mut" }, `${sum.problems || 0} problems`)),
      h("p", { class: "hx-note" }, fixes.length ? `${sum.answered} of them are answered by ${fixes.length} change${fixes.length === 1 ? "" : "s"}${ideas.length ? `, and ${ideas.length} more need an editor this build does not have` : ""}. ${sum.cost_ms ? `Together they cost you ${fmtMs(sum.cost_ms)}.` : ""}` : "No fix this week.")),
    h("div", { class: "hx-dock" }, applyBtn, tally,
      draft ? h("div", { class: "hx-est" }, h("span", {}, "into draft ", h("b", { class: "hx-num" }, draft.name))) : h("div", { class: "hx-est" }, h("span", {}, "current ", h("b", { class: "hx-num" }, d.current.name)), h("span", {}, "a new draft is made"))),
    back,
  ].filter(Boolean);

  host.replaceChildren(page({ kicker: `Optimize the harness · ${name} · ${d.current.name}`, title, sub, inspector: insp, cls: "hx-page" },
    tabs("fixes"),
    fixes.length ? h("section", { class: "hx-sec" }, label("Fixes, best first"), cards) : null,
    ideaList));
  sync();
}

useStyle("harness-fixes", `
.hx-fx.rec{border-color:var(--accent-line, var(--accent))}
.hx-fx .hx-ch{padding-bottom:0;flex-wrap:wrap;row-gap:var(--s-2)}
.hx-tick{display:inline-flex;align-items:center;gap:var(--s-3);cursor:pointer;min-height:var(--touch, 44px);white-space:nowrap}
.hx-fx .hx-ch input{width:18px;height:18px;flex:none;accent-color:var(--accent)}
.hx-rec{display:inline-flex;align-items:center;gap:4px;font:var(--w-mid) var(--fs-xs)/16px var(--mono);letter-spacing:.06em;text-transform:uppercase;color:var(--accent-text)}
.hx-ans{margin:var(--s-3) var(--s-4) 0;padding-top:var(--s-3);border-top:var(--hair) solid var(--line)}
.hx-ansh{font:var(--w-mid) var(--fs-xs)/16px var(--mono);letter-spacing:.06em;text-transform:uppercase;color:var(--text-2);margin-bottom:var(--s-2)}
.hx-ans ul{margin:0;padding:0;list-style:none;display:flex;flex-direction:column;gap:6px;font-size:var(--fs-sm);line-height:var(--lh-sm)}
.hx-ans a{color:var(--accent-text)}
.hx-fx .hx-what{padding-bottom:var(--s-3)}
.hx-fx .hx-ans{padding-bottom:var(--s-3)}
.hx-links{display:flex;flex-wrap:wrap;gap:var(--s-2)}
.hx-mut{color:var(--text-2)}
`);
