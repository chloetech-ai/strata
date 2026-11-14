// The "Blast radius" summary in an agent's Changes tab (SPEC.md, "Fourth
// build", use case 7): the risk line and what the change reaches (the
// functions, the tests that cover them, the decisions about its lines and
// the other agents in the way), from GET /api/agents/:id/blast, with the
// way to Ship. app.js mounts it once above the diff:
//   const s = mount(host, { api, go }); s.show(agentId, { fresh }); s.unmount();
import { h, useStyle, avatar, ctxChip } from "./common.js";

const enc = encodeURIComponent;
/** How long an answer is reused when the diff is read again. The server
 *  keeps one answer per set of facts (the diff, the branches, the test
 *  runs), so Ship and this row say the same risk for the same change. */
const KEEP_MS = 5_000;
const TONE = { low: "good", medium: "warn", high: "crit" };
const plural = (n, one, many = one + "s") => `${n} ${n === 1 ? one : many}`;

/** The decisions, counted as Review and Ship count them (the server's
 *  `decided`, from review::decisions_of): the ones that matter, the
 *  routine ones folded. An older server without it: the records about
 *  these lines. */
export function decisionsChip(b) {
  const d = b && b.decided;
  if (!d) {
    const n = (b && b.totals && b.totals.decisions) || 0;
    return n ? ctxChip(plural(n, "decision"), "decisions about these lines, from ChloeCtx") : h("span", {}, "no decisions");
  }
  if (!d.count) return h("span", {}, "no decisions");
  const why = d.matter ? `${d.matter} to confirm or that matter${d.routine ? `, ${d.routine} routine` : ""}` : `${d.routine} routine`;
  return ctxChip(plural(d.count, "decision"), `${why}: the same decisions Review and Ship list`);
}

export function mount(host, { api, go }) {
  const box = h("div", { class: "bs", "aria-live": "polite" });
  host.append(box);
  const cache = new Map(); // agent -> {at, data, err}
  let current = null, req = 0, want = null, seen = false;
  // Read only once the Changes tab shows it: zooming an agent mounts the
  // diff (for its count) before anyone looks at it.
  const io = typeof IntersectionObserver === "function" ? new IntersectionObserver((es) => {
    seen = es.some((e) => e.isIntersecting);
    if (seen && want) { const w = want; want = null; load(w); }
  }) : null;
  if (io) io.observe(host); else seen = true;

  function draw() {
    const c = cache.get(current);
    if (!current) { box.replaceChildren(); return; }
    if (!c || (!c.data && !c.err)) {
      box.replaceChildren(h("div", { class: "bs-row mut" }, h("span", { class: "bs-lab" }, "Blast radius"), h("span", { class: "bs-wait" }, "reading what this change reaches…")));
      return;
    }
    if (c.err) {
      box.replaceChildren(h("div", { class: "bs-row mut" }, h("span", { class: "bs-lab" }, "Blast radius"), h("span", { class: "bs-err", title: c.err }, c.err)));
      return;
    }
    const b = c.data, t = b.totals || {}, r = b.risk || {};
    if (!t.files) { box.replaceChildren(); return; }
    const untested = (b.functions || []).filter((f) => !(f.tests || []).length && f.status !== "deleted").length;
    box.replaceChildren(...[h("div", { class: `bs-row ${TONE[r.level] || "mut"}` },
      h("span", { class: "bs-dot" }),
      h("span", { class: "bs-lab" }, "Blast radius"),
      r.level ? h("b", { class: "bs-lvl" }, `${r.level[0].toUpperCase()}${r.level.slice(1)} risk`) : null,
      h("span", { class: "bs-nums" },
        h("span", {}, plural(t.functions || 0, "function")),
        h("span", {}, plural(t.tests || 0, "test")),
        untested ? h("span", { class: "bs-bad", title: `${untested} changed ${untested === 1 ? "function has" : "functions have"} no test that names ${untested === 1 ? "it" : "them"}` }, `${untested} untested`) : null,
        decisionsChip(b),
        h("span", { class: "bs-ags", title: (b.agents || []).map((g) => g.detail).join("\n") || null }, (b.agents || []).length ? [(b.agents || []).slice(0, 4).map((g) => avatar(g.agent, 16)), ` ${plural(b.agents.length, "agent")}`] : "no other agent")),
      h("span", { class: "bs-sp" }),
      h("button", { class: "bs-open", type: "button", onclick: () => go(`#/ship/${current}`) }, "Ship →")),
      r.line ? h("div", { class: "bs-line", title: (r.reasons || []).join("\n") }, r.line.replace(/^\w+ risk:\s*/i, ""), b.landing && b.landing.line ? ` ${b.landing.line}` : "") : null].flat().filter((k) => k != null && k !== false));
  }

  async function load(id) {
    const n = ++req;
    const prev = cache.get(id);
    cache.set(id, { at: Date.now(), data: prev && prev.data, err: null });
    try {
      const data = await api(`/api/agents/${enc(id)}/blast`);
      if (n !== req && current !== id) return;
      cache.set(id, { at: Date.now(), data, err: null });
    } catch (e) {
      const err = e && e.status === 404 && !/agent/i.test(e.message || "") ? "this server has no blast radius route" : (e && e.message) || String(e);
      cache.set(id, { at: Date.now(), data: prev && prev.data, err: prev && prev.data ? null : err });
    }
    if (current === id) draw();
  }

  return {
    show(id, { fresh = false } = {}) {
      current = id;
      const c = cache.get(id);
      draw();
      if (!c || (fresh && Date.now() - c.at > KEEP_MS)) {
        if (seen) load(id); else want = id;
      }
    },
    unmount() { if (io) io.disconnect(); box.remove(); cache.clear(); current = null; },
  };
}

useStyle("blast-summary", `
.bs{flex:none;border-bottom:1px solid var(--line);background:var(--surface-2)}
.bs:empty{display:none}
.bs-row{display:flex;align-items:center;gap:10px;padding:7px 12px 7px 14px;font-size:12.5px;color:var(--text-2);min-width:0}
.bs-row.mut{color:var(--muted)}
.bs-dot{width:8px;height:8px;border-radius:50%;flex:none;background:var(--muted)}
.bs-row.good .bs-dot{background:var(--good)}.bs-row.warn .bs-dot{background:var(--warn)}.bs-row.crit .bs-dot{background:var(--crit)}
.bs-lab{font-weight:650;color:var(--text);white-space:nowrap}
.bs-lvl{font-size:11px;font-weight:700;border-radius:999px;padding:1px 8px;white-space:nowrap}
.bs-row.good .bs-lvl{background:var(--good-soft);color:var(--good-ink)}.bs-row.warn .bs-lvl{background:var(--warn-soft);color:var(--warn-ink)}.bs-row.crit .bs-lvl{background:var(--crit-soft);color:var(--crit-ink)}
.bs-nums{display:flex;align-items:center;gap:12px;min-width:0;overflow:hidden}
.bs-nums>span{white-space:nowrap}
.bs-bad{color:var(--crit-ink);font-weight:600}
.bs-ags{display:inline-flex;align-items:center;gap:2px;white-space:nowrap}
.bs-ags .lv-av{font-size:9px;border-radius:5px;margin-right:1px}
.bs-sp{flex:1}
.bs-open{border:1px solid var(--line-2);background:var(--surface);color:var(--text);font:inherit;font-size:12px;font-weight:600;border-radius:7px;padding:3px 10px;cursor:pointer;white-space:nowrap}
.bs-open:hover{border-color:var(--accent);color:var(--accent)}
.bs-line{padding:0 14px 8px 32px;margin-top:-3px;font-size:12px;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.bs-wait,.bs-err{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
@media (max-width:640px){.bs-line{display:none}.bs-ags{display:none}}
`);
