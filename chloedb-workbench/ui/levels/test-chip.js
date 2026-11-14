// The chip on Story's steps that ran tests (SPEC.md, "Fourth build", row
// 5): what the runs of a step found, test by test, from
// `GET /api/tests/runs?agent=<id>` (one fetch per agent, kept 15 s), matched
// to the step by its events. "41 passed · 2 failed · 1 flaky"; a click opens
// Health's Tests tab. Story mounts it with
//   testChip(ctx, agentId, step)   -> a node (empty until the runs arrive)
import { h, useStyle } from "./common.js";

const cache = new Map(); // agent -> {at, runs | null, wait: Promise}

function runsOf(ctx, agent) {
  const c = cache.get(agent);
  if (c && (c.wait || Date.now() - c.at < 15000)) return c;
  const entry = { at: Date.now(), runs: c ? c.runs : null, wait: null };
  entry.wait = ctx.api(`/api/tests/runs?agent=${encodeURIComponent(agent)}`)
    .then((d) => { entry.runs = d.runs || []; }, () => { entry.runs = entry.runs || []; })
    .finally(() => { entry.at = Date.now(); entry.wait = null; });
  cache.set(agent, entry);
  return entry;
}

function fill(el, ctx, step, runs) {
  const seqs = new Set(step.events || []);
  const mine = runs.filter((r) => seqs.has(r.seq) || (r.seqs || []).some((s) => seqs.has(s)));
  if (!mine.length) { el.replaceChildren(); el.hidden = true; return; }
  const sum = (k) => mine.reduce((n, r) => n + (r[k] || 0), 0);
  const passed = sum("passed"), failed = sum("failed"), flaky = new Set(mine.flatMap((r) => r.flaky_tests || [])).size;
  const newly = new Set(mine.flatMap((r) => r.newly_failing || []));
  const failedNames = [...new Set(mine.flatMap((r) => r.failed_tests || []))];
  // Sixth build: a run that printed no count is only its exit code; none
  // is made up into a test.
  const unknown = mine.every((r) => r.counted === false || (r.counted == null && !r.tests && r.exit_code != null));
  const waited = mine.reduce((n, r) => n + (r.wait_ms || 0), 0);
  const parts = unknown
    ? [h("b", { class: mine.some((r) => r.exit_code) ? "f" : "p" }, mine.some((r) => r.exit_code) ? `exit ${mine.find((r) => r.exit_code).exit_code}` : "exit 0")]
    : [passed ? h("b", { class: "p" }, `${passed} passed`) : null, failed ? h("b", { class: "f" }, `${failed} failed`) : null, flaky ? h("b", { class: "w" }, `${flaky} flaky`) : null, newly.size ? h("b", { class: "f" }, `${newly.size} newly failing`) : null];
  const title = [
    `${mine.length} test run${mine.length === 1 ? "" : "s"} in this step`,
    failedNames.length ? `failed: ${failedNames.slice(0, 6).join(", ")}${failedNames.length > 6 ? ", …" : ""}` : null,
    waited >= 1000 ? `waited ${Math.round(waited / 1000)} s for a permission (not counted as test time)` : null,
    "Open Health's Tests",
  ].filter(Boolean).join("\n");
  el.hidden = false;
  el.title = title;
  el.replaceChildren(h("i", { class: "tc-ic" }), ...parts.filter(Boolean).flatMap((p, i) => (i ? [h("span", { class: "tc-dot" }, "·"), p] : [p])));
  el.onclick = (e) => { e.stopPropagation(); ctx.go("#/health/tests"); };
}

/** The chip for a Story step: empty and hidden unless the step ran tests
 *  the server parsed. */
export function testChip(ctx, agent, step) {
  const el = h("span", { class: "tc", role: "link", tabindex: "0", hidden: true });
  el.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.stopPropagation(); e.preventDefault(); el.click(); } });
  if (!agent || !step || step.kind !== "run") return el;
  const c = runsOf(ctx, agent);
  if (c.runs) fill(el, ctx, step, c.runs);
  if (c.wait) c.wait.then(() => fill(el, ctx, step, c.runs || []));
  return el;
}

useStyle("test-chip", `
.tc{display:inline-flex;align-items:center;gap:5px;flex:none;border:1px solid var(--line);border-radius:999px;padding:1px 8px 1px 6px;font-size:11.5px;background:var(--surface-2);cursor:pointer;white-space:nowrap}
.tc[hidden]{display:none}
.tc:hover,.tc:focus-visible{border-color:var(--line-2);outline:none}
.tc b{font-weight:650}.tc b.p{color:var(--good-ink)}.tc b.f{color:var(--crit-ink)}.tc b.w{color:var(--warn-ink)}
.tc-dot{color:var(--muted)}
.tc-ic{width:9px;height:9px;border-radius:2px;background:var(--lane-4);display:inline-block}
`);
