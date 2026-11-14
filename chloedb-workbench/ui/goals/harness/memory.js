// Optimize the harness, the memory editor (HARNESS.md, "Screens", 6): what
// ChloeCtx gives an agent, as scales (the gate, the knowledge threshold, the
// budgets), records pinned to the files they name, a live preview of one
// turn's recall under the draft, and every recall of the week recomputed
// from ChloeCtx's receipts. Exact for what is given; whether the model then
// uses it needs a replay.
import { h, useStyle, page, label, btn, bank, icon } from "/levels/common.js";
import { tabs, lv, gauge, rich, repoQ, toast, failed, loading, resultLine, saveDraft, promote } from "./parts.js";

const SCALES = [
  { key: "gate_hi", title: "Given in full from", unit: "", min: 3, max: 10, step: 0.05, fmt: (v) => v.toFixed(2), what: "A recall whose gate score reaches it gives its records in full." },
  { key: "gate_lo", title: "Given in brief from", unit: "", min: 2, max: 8, step: 0.05, fmt: (v) => v.toFixed(2), what: "Under it, nothing is given; between the two, a brief." },
  { key: "knowledge_min_cos", title: "Knowledge: any record at", unit: "", min: 0.2, max: 0.8, step: 0.01, fmt: (v) => v.toFixed(2), what: "Decisions, rules and facts that match the prompt on their own text." },
  { key: "budget_chars", title: "Budget a prompt", unit: "chars", min: 1000, max: 12000, step: 250, fmt: (v) => Math.round(v).toLocaleString("en"), what: "The most a recall gives, about a quarter of it in tokens." },
  { key: "briefing_chars", title: "Briefing at session start", unit: "chars", min: 2000, max: 12000, step: 250, fmt: (v) => Math.round(v).toLocaleString("en"), what: "What ChloeCtx gives before the first prompt." },
];

export function render(host, ctx) {
  host.replaceChildren(loading("Reading this week's receipts…"));
  const S = { alive: true, seq: 0, dirty: false };
  const load = () => ctx.api(`/api/harness/memory?${repoQ(ctx)}`).then(async (d) => {
    // The project's knowledge threshold as ChloeCtx has it now (Debug's, src/turns.rs).
    const th = await ctx.api(`/api/repos/${encodeURIComponent(d.repo)}/threshold`).catch(() => null);
    if (!S.alive) return;
    Object.assign(S, { d, th, repo: d.repo, v: d.version, edits: clone(d.edits), receipt: d.preview && d.preview.receipt, dirty: false });
    draw();
  }).catch((err) => { if (S.alive) host.replaceChildren(failed(err)); });
  // Promote sets the knowledge threshold through Debug's threshold API.
  async function applyThreshold(v) {
    if (v == null || !S.th || Math.abs(v - S.th.value) < 0.005) return;
    try {
      const r = await ctx.api(`/api/repos/${encodeURIComponent(S.repo)}/threshold`, { method: "PUT", body: { value: v } });
      toast(r.text || `Knowledge threshold set to ${v}.`);
    } catch (err) { toast(`The threshold was not set: ${err.message}`, "bad"); }
  }

  let timer = null;
  function changed() {
    S.dirty = true;
    clearTimeout(timer);
    timer = setTimeout(async () => {
      const n = ++S.seq;
      try {
        const d = await ctx.api("/api/harness/memory/preview", { method: "POST", body: { repo: S.repo, version: S.v.id, receipt: S.receipt, edits: S.edits } });
        if (n !== S.seq || !S.alive) return;
        S.d = { ...S.d, preview: d.preview, recompute: d.recompute, lint: d.lint };
        drawRight(); drawSide();
      } catch (err) { toast(err.message, "bad"); }
    }, 250);
  }
  async function save() {
    const r = await saveDraft(ctx, S.repo, S.v.id, { memory: S.edits });
    S.v = r.version; S.dirty = false;
    toast(`Saved to ${r.version.name}.`);
    return r.version;
  }
  async function doPromote() {
    try {
      const v = S.dirty || S.v.state === "current" ? await save() : S.v;
      if (v.state === "current") { toast("Nothing to promote: this is the current version."); return; }
      const r = await promote(ctx, v.id);
      if (!r) return;
      toast(`${r.version.name} is current: new agents get it.`);
      await applyThreshold(S.edits.knowledge_min_cos);
      load();
    } catch (err) { toast(err.status === 409 && err.code === "unchanged" ? "Nothing changed from the current version." : err.message, "bad"); }
  }

  let left, right, sideBox;
  function drawLeft() { left.replaceChildren(...leftPane(S, changed, drawLeft)); }
  function drawRight() { right.replaceChildren(...rightPane(S, (id) => { S.receipt = id; changed(); })); }
  function drawSide() { sideBox.replaceChildren(...side(S, { save: () => save().then(drawSide).catch((err) => toast(err.message, "bad")), promote: doPromote })); }
  function draw() {
    const d = S.d;
    left = h("div", { class: "pane hx-mleft" });
    right = h("div", { class: "pane hx-mright" });
    sideBox = h("div", { class: "hx-side" });
    host.replaceChildren(page({ kicker: `Optimize the harness · editing ${S.v.name}${S.v.state === "draft" ? `, from ${d.current.name}` : " (current)"}`, title: "Memory", sub: "What ChloeCtx gives an agent: the briefing it starts with, recall on each prompt, and records pinned to files. Every change is checked against this week's recalls, on the store as each one saw it.", inspector: sideBox, cls: "hx-page" },
      tabs("memory"),
      h("div", { class: "hx-grid2" }, left, right)));
    drawLeft(); drawRight(); drawSide();
  }
  load();
  return { unmount() { S.alive = false; clearTimeout(timer); } };
}

function clone(e) { return { gate_hi: null, gate_lo: null, knowledge_min_cos: null, budget_chars: null, briefing_chars: null, ...JSON.parse(JSON.stringify(e || {})), pins: [...((e && e.pins) || [])] }; }

function leftPane(S, changed, redraw) {
  const obs = S.d.observed || {};
  const scales = SCALES.map((sc) => {
    const set = S.edits[sc.key] != null;
    const val = set ? S.edits[sc.key] : obs[sc.key];
    const out = h("b", { class: "hx-num" }, `${sc.fmt(val)}${sc.unit ? " " + sc.unit : ""}`);
    const input = h("input", { type: "range", min: sc.min, max: sc.max, step: sc.step, value: val, "aria-label": sc.title });
    input.addEventListener("input", () => { S.edits[sc.key] = Number(input.value); out.textContent = `${sc.fmt(Number(input.value))}${sc.unit ? " " + sc.unit : ""}`; changed(); });
    input.addEventListener("change", redraw);
    return h("div", { class: `hx-scale${set ? " set" : ""}` },
      h("div", { class: "hx-sh" }, h("span", {}, sc.title), out),
      input,
      h("div", { class: "hx-sw" }, h("span", {}, sc.what, " ", set ? `ChloeCtx's: ${sc.fmt(obs[sc.key])}.` : "",
        sc.key === "knowledge_min_cos" && S.th ? ` The project's now: ${sc.fmt(S.th.value)}; Promote sets it.` : ""), set ? h("button", { type: "button", class: "lv-link", onclick: () => { S.edits[sc.key] = null; changed(); redraw(); } }, "Use ChloeCtx's") : null));
  });
  const recs = S.d.records || [];
  const recSel = h("select", { class: "field", "aria-label": "A record to pin" }, h("option", { value: "" }, recs.length ? "A decision, rule or fact from this week's recalls…" : "No records in this week's receipts"), recs.map((r) => h("option", { value: r.id }, `${r.kind} · ${r.text.slice(0, 80)}`)));
  const fileIn = h("input", { class: "field", placeholder: "the file it is about (e.g. cart.py)", "aria-label": "File" });
  recSel.addEventListener("change", () => { const r = recs.find((x) => x.id === recSel.value); if (r && r.files && r.files[0] && !fileIn.value) fileIn.value = r.files[0]; });
  const addPin = () => {
    const r = recs.find((x) => x.id === recSel.value);
    if (!r) { toast("Pick a record to pin."); return; }
    if (!fileIn.value.trim()) { toast("Name the file it is about."); return; }
    if (!S.edits.pins.some((p) => p.record === r.id && p.file === fileIn.value.trim())) S.edits.pins.push({ file: fileIn.value.trim(), record: r.id, text: r.text });
    changed(); redraw();
  };
  const base = (S.d.base && S.d.base.pins) || [];
  return [
    h("div", { class: "hd" }, label("Recall", h("span", { class: "sp" }), h("span", { class: "hx-mut hx-rule-word" }, obs.from_receipts ? "ChloeCtx's from its receipts" : "ChloeCtx's defaults"))),
    h("div", { class: "bd" }, scales),
    h("div", { class: "hd hx-hd2" }, label("Pins", h("span", { class: "hx-num" }, String(S.edits.pins.length)))),
    h("div", { class: "hx-pins" },
      S.edits.pins.length ? S.edits.pins.map((p, i) => h("div", { class: `hx-pin${base.some((b) => b.record === p.record) ? "" : " new"}` },
        h("code", {}, p.file),
        h("span", {}, base.some((b) => b.record === p.record) ? null : h("b", {}, "New: "), `“${p.text}”`),
        h("button", { type: "button", class: "hx-x", "aria-label": `Unpin from ${p.file}`, onclick: () => { S.edits.pins.splice(i, 1); changed(); redraw(); } }, icon("x", "i sm"))))
        : h("p", { class: "hx-note hx-padx" }, "No pins. A pinned record is given at the start of every new agent's session, whatever its prompt scores."),
      h("div", { class: "hx-pinadd" }, recSel, fileIn, h("button", { type: "button", class: "btn", onclick: addPin }, "Pin"))),
  ];
}

function rightPane(S, pickTurn) {
  const p = S.d.preview;
  const rc = S.d.recompute || {};
  const turns = S.d.turns || [];
  const vName = S.v.state === "draft" ? S.v.name : "draft";
  const curName = S.d.current.name;
  const sel = h("select", { class: "field hx-turn", "aria-label": "A turn" }, turns.map((t) => h("option", { value: t.receipt, selected: p && t.receipt === p.receipt ? true : null }, `${t.name} turn ${t.turn}${t.missed ? " · missed" : ""} · ${t.query}`)));
  sel.addEventListener("change", () => pickTurn(sel.value));
  const out = [];
  out.push(h("div", { class: "hd" }, label(p ? `Live preview · ${p.name} turn ${p.turn}` : "Live preview", h("span", { class: "sp" }), turns.length ? h("span", { class: "hx-mut hx-rule-word" }, `${turns.length} turns`) : null)));
  if (!p) {
    out.push(h("div", { class: "bd hx-mut" }, (S.d.missing || []).length ? "ChloeCtx's receipts could not be read for this week's sessions." : "No recall receipts this week: agents started with ChloeCtx write one per prompt."));
  } else {
    out.push(h("div", { class: "bd" }, turns.length > 1 ? sel : null, h("div", { class: "lbl hx-gap" }, "Prompt"), h("p", { class: "hx-q" }, `“${p.query}”`)));
    out.push(h("table", { class: "hx-t hx-prev" },
      h("thead", {}, h("tr", {}, h("th", {}, "Record"), h("th", { class: "r" }, "Score"), h("th", {}, curName), h("th", {}, vName))),
      h("tbody", {}, p.rows.slice(0, 14).map((r) => h("tr", { class: r.changed ? "hx-chg" : null },
        h("td", { "data-k": "rec" }, h("div", { class: r.changed ? "hx-strong" : null }, r.text.length > 160 ? r.text.slice(0, 160) + "…" : r.text), h("small", { class: "hx-mut" }, [r.kind, r.tag].filter(Boolean).join(" · "))),
        h("td", { class: "r hx-num", "data-k": "score" }, r.score == null ? "–" : Number(r.score).toFixed(2)),
        h("td", { "data-k": "a" }, word(r.before)),
        h("td", { "data-k": "b" }, word(r.after)))))));
    if (!p.exact) out.push(h("p", { class: "hx-note hx-padx" }, "Which records a changed budget gives is estimated from the candidates in rank order."));
  }
  out.push(h("div", { class: "hd hx-hd2" }, label("Every recall this week, again", h("span", { class: "sp" }), lv(rc.level === "none" ? "none" : "recomputed"))));
  const fixed = (rc.misses_fixed || []);
  out.push(h("div", { class: "bd" },
    bank(3,
      gauge({ label: "Recalls", v: String(rc.recalls || 0), on: rc.recalls_exact, total: rc.recalls, delta: `${rc.sessions || 0} sessions` }),
      gauge({ label: "Records added", v: `${rc.added >= 0 ? "+" : ""}${rc.added || 0}`, on: rc.added, total: Math.max(rc.recalls || 0, 1), delta: `${(rc.sessions_changed || []).length} sessions · ${rc.tokens >= 0 ? "+" : ""}${rc.tokens || 0} tok${rc.dropped ? ` · −${rc.dropped} records` : ""}` }),
      gauge({ label: "Misses fixed", v: String(fixed.length), on: fixed.length, total: Math.max((S.d.misses || []).length, 1), tone: fixed.length ? "good" : "", delta: fixed.length ? fixed.map((m) => `${m.agent} t${m.turn}`).join(", ") : `of ${(S.d.misses || []).length}` })),
    rc.result === "no_difference" ? resultLine("no_difference", `Every one of the ${rc.recalls} recalls gives what it gave under ${curName}. Each is recomputed from its own receipt, so a change of one record would show.`) : null,
    h("p", { class: "hx-note" }, rc.note || "")));
  return out;
}

function word(w) { return h("span", { class: `hx-w ${String(w).replace(/ /g, "-")}` }, w); }

function side(S, act) {
  const misses = S.d.misses || [];
  const lint = S.d.lint || [];
  return [
    h("div", {}, label("Lint", h("span", { class: "hx-num" }, String(misses.length + lint.length))),
      misses.length || lint.length ? h("div", { class: "hx-lint" },
        misses.map((m) => h("div", { class: "hx-li bad" }, icon("x"), h("div", {}, h("b", {}, "A record was missed where it applied"),
          h("p", {}, `${m.name} edited ${m.file} at turn ${m.turn}; “${m.text.slice(0, 120)}” scored ${Number(m.score).toFixed(2)} and was left out. ${m.failed} test run${m.failed === 1 ? "" : "s"} failed after it. `, lv(m.level))))),
        lint.map((f) => h("div", { class: "hx-li bad" }, icon("x"), h("div", {}, h("b", {}, "A secret in the harness"), h("p", {}, rich(f.message))))))
        : h("p", { class: "hx-note" }, "No missed memory this week, and no secret in the part.")),
    h("div", {}, label("How it reaches agents"), h("p", { class: "hx-note" }, S.d.applied)),
    h("div", { class: "hx-dock" },
      btn(S.v.state === "draft" && !S.dirty ? `Saved in ${S.v.name}` : `Save to ${S.v.state === "draft" ? S.v.name : "a draft"}`, { primary: true, kbd: "⌘S", disabled: !S.dirty, onclick: act.save }),
      btn("Promote: new agents get it", { disabled: !(S.dirty || S.v.state === "draft"), onclick: act.promote })),
  ];
}

useStyle("harness-memory", `
.hx-hd2{border-top:var(--hair) solid var(--line)}
.hx-scale{padding:var(--s-3) 0;border-bottom:var(--hair) solid var(--line)}
.hx-scale:last-child{border-bottom:0}
.hx-sh{display:flex;justify-content:space-between;gap:var(--s-3);font-size:var(--fs-md);font-weight:var(--w-mid)}
.hx-scale.set .hx-sh b{color:var(--accent-text)}
.hx-scale input[type=range]{width:100%;margin:var(--s-3) 0 var(--s-2);accent-color:var(--accent);min-height:24px}
.hx-sw{display:flex;justify-content:space-between;gap:var(--s-3);color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.hx-sw .lv-link{flex:none;font-size:var(--fs-sm)}
.hx-pins{display:flex;flex-direction:column}
.hx-pin{display:grid;grid-template-columns:minmax(80px,auto) minmax(0,1fr) auto;gap:var(--s-3);align-items:center;padding:var(--s-2) var(--s-4);border-bottom:var(--hair) solid var(--line);font-size:var(--fs-sm)}
.hx-pin.new{background:var(--good-soft)}.hx-pin.new b{color:var(--good-ink)}
.hx-pin code{overflow-wrap:anywhere}
.hx-pinadd{display:grid;grid-template-columns:minmax(0,1.4fr) minmax(0,1fr) auto;gap:var(--s-2);padding:var(--s-3) var(--s-4)}
.hx-padx{padding:0 var(--s-4)}
.hx-q{margin:var(--s-1) 0 0;font-size:var(--fs-md);line-height:var(--lh-md)}
.hx-turn{font-size:var(--fs-sm)}
.hx-prev td[data-k=rec]{overflow-wrap:anywhere}
.hx-prev small{display:block;margin-top:2px}
.hx-prev tr.hx-chg td{background:var(--accent-soft)}
.hx-strong{font-weight:var(--w-mid)}
.hx-w{font:var(--w-mid) var(--fs-xs)/16px var(--mono);letter-spacing:.06em;text-transform:uppercase;white-space:nowrap;color:var(--text)}
.hx-w.left-out{color:var(--crit-ink)}.hx-w.pinned{color:var(--good-ink)}.hx-w.not-a-candidate{color:var(--text-3)}
@media (max-width:640px){
  .hx-pinadd{grid-template-columns:minmax(0,1fr)}
  .hx-pin{grid-template-columns:minmax(0,1fr) auto}.hx-pin code{grid-column:1 / -1}
  .hx-prev tr{grid-template-columns:minmax(0,1fr) auto auto !important}
  .hx-prev td[data-k=rec]{grid-column:1 / -1}
}
`);
