// Trace (SPEC.md, "Trace (as built)", level Plan): a question in a sentence
// ("when and why did we change x to y, and how does it work now?") answered
// with a dossier of where it happened, without a model: the reading of the
// question as chips you can fix, a one-line answer assembled from the
// evidence, a timeline of evidence cards (the commits where x became y, on
// every branch; edits seen in Workbench's events; ChloeCtx's ◆ decisions;
// your answers and rules from Decide; the goal the agent was on), each with
// links to the state then (Story, Rewind, Mission and Map as of that
// moment), and the code as it is now. "Explain" asks ChloeCtx with a model,
// only when clicked. Other screens open it as #/trace?q=<question>.
import {
  h, useStyle, toMs, fmtClock, fmtDay, agentChip, avatar, ctxChip, pill, repoOf, onEscape, setFleet,
} from "./common.js";
// Sixth build: no repository yet is a picker (ui/levels/repos.js).
import { repoPicker, needsRepo } from "./repos.js";

const enc = encodeURIComponent;
const plural = (n, one, many = one + "s") => `${n} ${n === 1 ? one : many}`;
const KINDS = ["when", "why", "how", "who"];
const KIND_HELP = {
  when: "the commits and edits where it changed",
  why: "decisions, your answers, rules and the goal behind them",
  how: "the code as it is now",
  who: "the agents and people involved",
};
const ITEM = {
  commit: { label: "Commit", tone: "accent", group: "when" },
  edit: { label: "Edit", tone: "accent", group: "when" },
  decision: { label: "Decision", tone: "ctx", group: "why" },
  question: { label: "You answered", tone: "good", group: "why" },
  rule: { label: "Rule", tone: "good", group: "why" },
  goal: { label: "Goal", tone: "warn", group: "why" },
};
const EXAMPLES = [
  "when and why did we change the cart total rounding from floor to half-up, and how does it work now?",
  "why is `retries` 5?",
  "who changed api/src/money.rs and why?",
  "when did cart_total start taking discounts?",
];

const svg = (d, cls = "tr-i") => {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("class", cls);
  s.setAttribute("aria-hidden", "true");
  s.innerHTML = d;
  return s;
};
const I = {
  search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-4-4"/>',
  commit: '<circle cx="12" cy="12" r="3.5"/><path d="M3 12h5.5M15.5 12H21"/>',
  edit: '<path d="M4 20h4L19 9l-4-4L4 16z"/><path d="M13.5 6.5l4 4"/>',
  decision: '<path d="M12 3l7 9-7 9-7-9z"/>',
  question: '<path d="M4 5h16v11H9l-5 4z"/><path d="M10 9.5a2 2 0 1 1 2.6 1.9c-.5.2-.6.6-.6 1.1M12 14.5h.01"/>',
  rule: '<path d="M5 4h14v16H5z"/><path d="M9 9h6M9 13h6M9 17h3"/>',
  goal: '<circle cx="12" cy="12" r="8.5"/><circle cx="12" cy="12" r="4.5"/><circle cx="12" cy="12" r="1" fill="currentColor"/>',
  play: '<path d="M8 5.5v13l10.5-6.5z" fill="currentColor"/>',
  book: '<path d="M4 5.5A2.5 2.5 0 0 1 6.5 3H20v15H6.5A2.5 2.5 0 0 0 4 20.5z"/><path d="M4 20.5A2.5 2.5 0 0 0 6.5 23H20v-5"/>',
  map: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M11 3v18M11 12h10M16 12v9"/>',
  target: '<circle cx="12" cy="12" r="8.5"/><circle cx="12" cy="12" r="4.5"/>',
  diff: '<path d="M6 3v12M3 12l3 3 3-3M18 21V9M15 12l3-3 3 3"/>',
  why: '<circle cx="6" cy="6" r="2.2"/><circle cx="6" cy="18" r="2.2"/><circle cx="18" cy="12" r="2.2"/><path d="M6 8.2v7.6M8 7c5 1 7.5 2.3 8.2 3.6M8 17c5-1 7.5-2.3 8.2-3.6"/>',
  doc: '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  spark: '<path d="M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5L18 18M6 18l2.5-2.5M15.5 8.5L18 6"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v5.5M12 7.6h.01"/>',
};

/** A needle as a forgiving pattern: `half-up` finds half_up, halfUp, half up. */
function needleRe(n) {
  const parts = String(n || "").split(/[-_\s]+|(?<=[a-z0-9])(?=[A-Z])/).filter(Boolean).map((p) => p.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  return parts.length ? parts.join("[-_ ]?") : null;
}

/** Text with x and y marked (x as what it was, y as what it became). */
function marked(text, reading) {
  const xs = [];
  if (reading?.x) xs.push(["x", needleRe(reading.x)]);
  if (reading?.y) xs.push(["y", needleRe(reading.y)]);
  for (const s of reading?.subjects || []) xs.push(["s", needleRe(s)]);
  const pats = xs.filter(([, r]) => r);
  if (!pats.length) return [String(text ?? "")];
  const re = new RegExp(pats.map(([k, r]) => `(?<${k}${Math.random().toString(36).slice(2, 6)}>${r})`).join("|"), "gi");
  const out = [];
  let at = 0;
  const s = String(text ?? "");
  for (const m of s.matchAll(re)) {
    if (m.index > at) out.push(s.slice(at, m.index));
    const k = Object.entries(m.groups || {}).find(([, v]) => v != null)?.[0]?.[0] || "s";
    out.push(h("mark", { class: `tr-m tr-m-${k}` }, m[0]));
    at = m.index + m[0].length;
  }
  if (at < s.length) out.push(s.slice(at));
  return out;
}

function when(ms) {
  if (!ms) return "";
  const day = fmtDay(ms);
  const d = new Date(ms);
  const date = d.toLocaleDateString([], { month: "short", day: "numeric" });
  return `${day === "today" || day === "yesterday" ? day : `on ${date}`} at ${fmtClock(ms)}`;
}

const shortSha = (s) => String(s || "").slice(0, 7);
const base = (p) => String(p || "").split("/").pop();

/** `#/trace?q=…` → its parameters. */
function hashParams() {
  const i = location.hash.indexOf("?");
  return new URLSearchParams(i >= 0 ? location.hash.slice(i + 1) : "");
}

export function mount(root, ctx) {
  const hp = hashParams();
  const S = {
    q: (ctx.params && ctx.params.q) || hp.get("q") || "",
    repo: (ctx.params && ctx.params.repo) || hp.get("repo") || repoOf(ctx),
    data: null, err: null, busy: false, over: null, kinds: null,
    editing: null, explain: null, explaining: false, explainErr: null,
    open: new Set(), diffs: new Map(), whys: new Map(), timeApi: null, flash: null,
  };
  const el = h("div", { class: "lv-screen tr" });
  root.append(el);
  let alive = true, req = 0, dropEsc = null;

  // ------------------------------------------------------------- data
  function query() {
    const p = new URLSearchParams({ q: S.q });
    if (S.repo) p.set("repo", S.repo);
    if (S.over) for (const [k, v] of Object.entries(S.over)) p.set(k, Array.isArray(v) ? v.join(",") : v ?? "");
    return p.toString();
  }

  async function run({ keepHash = false } = {}) {
    const q = S.q.trim();
    if (!q) { S.data = null; render(); return; }
    if (!keepHash) {
      const hash = `#/trace?q=${enc(q)}`;
      if (location.hash !== hash) history.replaceState(history.state, "", hash);
    }
    const my = ++req;
    S.busy = true; S.err = null; S.explain = null; S.explainErr = null; S.open.clear(); S.diffs.clear(); S.whys.clear();
    render();
    try {
      const d = await ctx.api(`/api/trace?${query()}`);
      if (my !== req || !alive) return;
      S.data = d;
      S.kinds = new Set(d.reading?.kinds?.length ? d.reading.kinds : ["when", "why", "how"]);
      setFleet((ctx.state && ctx.state.agents) || []);
      probeTime(d.links?.time_ms);
    } catch (e) {
      if (my !== req || !alive) return;
      S.err = e.message || String(e);
    }
    S.busy = false;
    render();
  }

  // Time travel is another part's (GET /api/time?at=): show Mission and
  // Map as of a moment only when the route answers.
  async function probeTime(ms) {
    if (S.timeApi != null || !ms) return;
    try { await ctx.api(`/api/time?at=${Math.round(ms)}${S.repo ? `&repo=${enc(S.repo)}` : ""}`); S.timeApi = true; }
    catch (e) { S.timeApi = e.status === 404 ? false : true; }
    if (alive) render();
  }

  async function explain() {
    if (S.explaining) return;
    S.explaining = true; S.explainErr = null;
    render();
    try {
      const body = { q: S.q, repo: S.repo || null, ...(S.over || {}) };
      S.explain = await ctx.api("/api/trace/explain", { method: "POST", body });
    } catch (e) { S.explainErr = e.message || String(e); }
    S.explaining = false;
    if (alive) render();
  }

  async function toggleDiff(it) {
    const sha = it.links?.diff?.sha;
    if (!sha) return;
    if (S.diffs.has(it.id)) { S.diffs.delete(it.id); render(); return; }
    S.diffs.set(it.id, { loading: true });
    render();
    try {
      const d = await ctx.api(`/api/trace/commit?sha=${enc(sha)}${S.data?.repo || S.repo ? `&repo=${enc(S.data?.repo || S.repo)}` : ""}`);
      S.diffs.set(it.id, { d });
    } catch (e) { S.diffs.set(it.id, { err: e.message || String(e) }); }
    if (alive) render();
  }

  // The Why card: the app's own for a line of an agent's worktree; for a
  // line of the main checkout, a short one here from GET /api/why?repo=.
  async function why(w, key, text) {
    if (!w) return;
    // A line of a commit card is read and blamed as that commit has it.
    if (w.agent && ctx.openWhy) { ctx.openWhy({ agent: w.agent, path: w.path, line: w.line, text, commit: w.commit || undefined }); return; }
    if (S.whys.has(key)) { S.whys.delete(key); render(); return; }
    S.whys.set(key, { loading: true, w });
    render();
    try {
      const d = await ctx.api(`/api/why?repo=${enc(w.repo || S.data?.repo || S.repo)}&path=${enc(w.path)}&line=${w.line}${w.commit ? `&commit=${enc(w.commit)}` : ""}`);
      S.whys.set(key, { d, w });
    } catch (e) { S.whys.set(key, { err: e.message || String(e), w }); }
    if (alive) render();
  }

  // ------------------------------------------------------------ editing
  function current() {
    const r = S.data?.reading || {};
    return {
      x: r.x || "", y: r.y || "", topic: r.topic || "", in: (r.paths || []).join(","),
      subject: (r.subjects || []).join(","), kinds: [...(S.kinds || [])].join(","),
    };
  }
  function setChip(key, value) {
    const o = { ...current(), ...(S.over || {}) };
    if (key === "subject" && S.editing?.index != null) {
      const list = (o.subject ? String(o.subject).split(",") : []).filter(Boolean);
      if (value) list[S.editing.index] = value; else list.splice(S.editing.index, 1);
      o.subject = list.join(",");
    } else if (key === "in" && S.editing?.index != null) {
      const list = (o.in ? String(o.in).split(",") : []).filter(Boolean);
      if (value) list[S.editing.index] = value; else list.splice(S.editing.index, 1);
      o.in = list.join(",");
    } else o[key] = value;
    S.over = o; S.editing = null;
    run({ keepHash: true });
  }
  function addChip(text) {
    const t = text.trim();
    if (!t) { S.editing = null; render(); return; }
    const o = { ...current(), ...(S.over || {}) };
    let m;
    if ((m = t.match(/^(x|from)\s*[:=]?\s+(.+)$/i))) o.x = m[2];
    else if ((m = t.match(/^(y|to)\s*[:=]?\s+(.+)$/i))) o.y = m[2];
    else if ((m = t.match(/^in\s+(.+)$/i))) o.in = [o.in, m[1]].filter(Boolean).join(",");
    else if ((m = t.match(/^(about|topic)\s*[:=]?\s+(.+)$/i))) o.topic = m[2];
    else o.subject = [o.subject, t].filter(Boolean).join(",");
    S.over = o; S.editing = null;
    run({ keepHash: true });
  }
  function toggleKind(k) {
    if (!S.kinds) S.kinds = new Set(["when", "why", "how"]);
    if (S.kinds.has(k)) { if (S.kinds.size > 1) S.kinds.delete(k); } else S.kinds.add(k);
    render();
  }

  // ------------------------------------------------------------- views
  function askBox() {
    const input = h("input", {
      class: "tr-q", type: "search", value: S.q, "aria-label": "Your question", spellcheck: "false", autocomplete: "off",
      placeholder: "Ask when, why or how something changed: “when and why did we change retries from 3 to 5?”",
      oninput: (e) => { S.q = e.target.value; },
    });
    import("./complete.js").then((m) => m.attachComplete(input, { field: "trace", ctx })).catch(() => {}); // Fifth build: autocomplete
    return h("form", { class: "tr-ask", onsubmit: (e) => { e.preventDefault(); S.over = null; run(); } },
      svg(I.search, "tr-i tr-ask-i"), input,
      h("button", { class: "btn primary tr-go", type: "submit", disabled: S.busy || null }, S.busy ? "Tracing…" : "Trace"));
  }

  function chip(key, label, text, index) {
    const editingThis = S.editing && S.editing.key === key && (S.editing.index ?? null) === (index ?? null);
    if (editingThis) {
      const inp = h("input", {
        class: "tr-chip-in", value: text, "aria-label": `${label}`,
        onkeydown: (e) => {
          if (e.key === "Enter") { e.preventDefault(); setChip(key, e.target.value.trim()); }
          if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); S.editing = null; render(); }
        },
        onblur: (e) => { if (S.editing && S.editing.key === key) setChip(key, e.target.value.trim()); },
      });
      queueMicrotask(() => { inp.focus(); inp.select(); });
      return h("span", { class: `tr-chip tr-c-${key} editing` }, h("b", null, label), inp);
    }
    return h("span", { class: `tr-chip tr-c-${key}` },
      h("button", { class: "tr-chip-b", type: "button", title: "Click to fix it", onclick: () => { S.editing = { key, index }; render(); } },
        h("b", null, label), h("span", { class: key === "x" || key === "y" || key === "subject" ? "mono" : "" }, text)),
      h("button", { class: "tr-chip-del", type: "button", "aria-label": `Remove ${label} ${text}`, onclick: () => { S.editing = { key, index }; setChip(key, ""); } }, svg(I.x)));
  }

  function readingRow() {
    const r = S.data?.reading;
    if (!r) return null;
    const kinds = S.kinds || new Set(r.kinds || []);
    const chips = [];
    if (r.x) chips.push(chip("x", "x", r.x));
    if (r.x && r.y) chips.push(h("span", { class: "tr-arrow", "aria-hidden": "true" }, "→"));
    if (r.y) chips.push(chip("y", "y", r.y));
    (r.subjects || []).forEach((s, i) => chips.push(chip("subject", "subject", s, i)));
    if (r.topic) chips.push(chip("topic", "about", r.topic));
    (r.paths || []).forEach((p, i) => chips.push(chip("in", "in", p, i)));
    const adding = S.editing && S.editing.key === "add";
    chips.push(adding
      ? (() => {
        const inp = h("input", { class: "tr-chip-in wide", placeholder: "x floor · y half-up · in web/ · an identifier", onkeydown: (e) => {
          if (e.key === "Enter") { e.preventDefault(); addChip(e.target.value); }
          if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); S.editing = null; render(); }
        }, onblur: (e) => addChip(e.target.value) });
        queueMicrotask(() => inp.focus());
        return h("span", { class: "tr-chip editing" }, inp);
      })()
      : h("button", { class: "tr-add", type: "button", onclick: () => { S.editing = { key: "add" }; render(); } }, svg(I.plus), "fix the reading"));
    return h("div", { class: "tr-reading" },
      h("span", { class: "tr-lbl" }, "Reading"),
      h("span", { class: "tr-kinds", role: "group", "aria-label": "What you asked" },
        KINDS.map((k) => h("button", { class: "tr-kind", type: "button", "aria-pressed": String(kinds.has(k)), title: KIND_HELP[k], onclick: () => toggleKind(k) }, k))),
      h("span", { class: "tr-sep" }),
      chips,
      r.needles?.length ? h("span", { class: "tr-searched" }, "searched ", r.needles.map((n) => h("code", { class: "lv-code" }, n)), " in git log -G --all") : null);
  }

  function answerCard() {
    const d = S.data;
    if (!d) return null;
    const a = d.answer;
    const ev = a?.evidence || {};
    const counts = [["commits", "commit"], ["edits", "edit"], ["decisions", "decision"], ["questions", "answer"], ["rules", "rule"]]
      .filter(([k]) => ev[k]).map(([k, one]) => plural(ev[k], one));
    const sentence = a ? a.text.replace("{when}", when(a.when_ms)) : "Nothing in git, Workbench's events or ChloeCtx matches this yet. Fix the reading above, or name the identifier or file.";
    const key = a?.key ? d.timeline.find((t) => t.id === a.key) : null;
    return h("section", { class: `tr-answer${a ? "" : " none"}` },
      h("div", { class: "tr-answer-top" },
        h("span", { class: "tr-answer-i" }, svg(a ? I.target : I.info)),
        h("p", { class: "tr-answer-t" }, marked(sentence, d.reading))),
      (a?.history || []).length >= 2 ? h("p", { class: "tr-history" },
        h("b", null, `Changed ${a.history.length} times: `),
        a.history.map((x, i) => [i ? h("span", { class: "tr-arrow", "aria-hidden": "true" }, " → ") : null,
          h("button", { class: "tr-hist", type: "button", title: `${x.by ? x.by + " · " : ""}${new Date(x.ts_ms).toLocaleString()}`, onclick: () => focusItem(x.id) },
            marked(x.label, d.reading), h("span", { class: "tr-mut" }, ` (${new Date(x.ts_ms).toLocaleDateString([], { month: "short", day: "numeric" })})`))])) : null,
      h("div", { class: "tr-answer-meta" },
        counts.length ? h("span", null, "from ", counts.join(" · ")) : null,
        key ? h("button", { class: "lv-link", type: "button", onclick: () => focusItem(key.id) }, "go to the change") : null,
        h("span", { class: "tr-grow" }),
        d.ms != null ? h("span", { class: "tr-mut" }, `${d.ms} ms · no model`) : null,
        h("button", { class: "btn tr-explain", type: "button", disabled: S.explaining || !d.timeline?.length || null, onclick: explain, title: "Asks `chloectx ask --deep`, which calls a model, for a fuller account citing this evidence" },
          svg(I.spark), S.explaining ? "Explaining…" : "Explain", h("span", { class: "tr-explain-n" }, "calls a model"))),
      S.explaining ? h("div", { class: "tr-explained loading" }, h("span", { class: "tr-spin" }), "ChloeCtx is reading the evidence with a model…") : null,
      S.explainErr ? h("div", { class: "tr-explained bad" }, S.explainErr) : null,
      S.explain ? explained(S.explain) : null);
  }

  function explained(x) {
    const text = x.answer || x.text || "";
    const parts = [];
    let at = 0;
    for (const m of text.matchAll(/\[(E\d+(?:\s*,\s*E\d+)*)\]/g)) {
      if (m.index > at) parts.push(text.slice(at, m.index));
      for (const tag of m[1].split(/\s*,\s*/)) {
        const n = Number(tag.slice(1)) - 1;
        const it = S.data?.timeline?.[n];
        parts.push(h("button", { class: "tr-cite", type: "button", title: it ? it.title : tag, onclick: () => it && focusItem(it.id) }, tag));
      }
      at = m.index + m[0].length;
    }
    if (at < text.length) parts.push(text.slice(at));
    return h("div", { class: "tr-explained" },
      h("div", { class: "tr-explained-h" }, ctxChip("ChloeCtx, with a model"),
        Number.isFinite(Number(x.confidence)) && x.confidence !== null && x.confidence !== "" ? h("span", { class: "tr-mut" }, `confidence ${Math.round(Number(x.confidence) * 100)}%`) : null,
        x.cost_usd != null ? h("span", { class: "tr-mut" }, `$${Number(x.cost_usd).toFixed(3)}`) : null,
        x.ms != null ? h("span", { class: "tr-mut" }, `${(x.ms / 1000).toFixed(1)} s`) : null),
      h("p", null, parts),
      (x.missing || []).length ? h("p", { class: "tr-mut" }, x.missing.join(" ")) : null);
  }

  function focusItem(id) {
    const node = el.querySelector(`[data-item="${CSS.escape(id)}"]`);
    if (!node) return;
    node.scrollIntoView({ block: "center", behavior: "smooth" });
    node.classList.remove("flash");
    void node.offsetWidth;
    node.classList.add("flash");
  }

  function agentOf(it) {
    const a = it.agent;
    if (!a) return h("span", { class: "tr-by" }, avatar(it.by === "you" ? "you" : { name: it.by || "?" }, 18), h("span", null, it.by === "you" ? "you" : it.by || "someone"));
    return agentChip(a, ctx);
  }

  function changePill(it, r) {
    if (it.kind === "edit" && it.committed === false) return pill("not committed", "warn");
    if (it.change === "changed" && r.x && r.y) return h("span", { class: "tr-change" }, h("mark", { class: "tr-m tr-m-x" }, r.x), " → ", h("mark", { class: "tr-m tr-m-y" }, r.y));
    if (it.change === "added") return pill(`brought in ${r.y && it.lines?.some((l) => new RegExp(needleRe(r.y), "i").test(l.text)) ? r.y : r.x || "it"}`, "mut");
    if (it.change === "removed") return pill(`took out ${r.x || "it"}`, "mut");
    return null;
  }

  function diffLines(lines, r) {
    if (!lines?.length) return null;
    let last = null;
    return h("div", { class: "tr-lines" }, lines.map((l) => {
      const head = l.path !== last ? h("div", { class: "tr-lines-f" }, l.path) : null;
      last = l.path;
      return [head, h("div", { class: `tr-ln s${l.sign === "+" ? "p" : l.sign === "-" ? "m" : "c"}` },
        h("span", { class: "tr-ln-n" }, l.line ?? ""), h("span", { class: "tr-ln-s" }, l.sign), h("code", null, marked(l.text, r)))];
    }));
  }

  function linkBtn(icon, text, onclick, title) {
    return h("button", { class: "tr-act", type: "button", onclick, title: title || null }, svg(icon), text);
  }

  function actions(it) {
    const L = it.links || {};
    const out = [];
    if (L.diff) out.push(linkBtn(I.diff, S.diffs.has(it.id) ? "Hide diff" : "Diff", () => toggleDiff(it), `git show ${shortSha(L.diff.sha)}`));
    // A commit's line is read and blamed as that commit has it (its line number is the commit's).
    if (L.why) out.push(linkBtn(I.why, `Why? ${base(L.why.path)}:${L.why.line}`, () => why(it.kind === "commit" && it.commit?.sha ? { ...L.why, commit: it.commit.sha } : L.why, it.id, it.lines?.find((l) => l.sign === "+")?.text), "The decision trail of this line"));
    if (L.story) out.push(linkBtn(I.book, `Story at ${fmtClock(L.story.at_ms)}`, () => ctx.go(`#/story/${L.story.agent}`), "The agent's story, at that turn"));
    if (L.rewind) out.push(linkBtn(I.play, "Rewind", () => ctx.openRewind({ agent: L.rewind.agent, from_ms: L.rewind.from_ms, to_ms: L.rewind.to_ms, edit_ms: L.rewind.to_ms - 3000, label: `${it.agent?.name || ""}: ${it.title}` }), "Its terminal, 20 s before to 3 s after"));
    // Snapshots are kept a day (time travel), so older moments get no link.
    if (S.timeApi && L.time_ms && it.kind !== "goal" && Date.now() - L.time_ms < 864e5) {
      out.push(linkBtn(I.target, "Mission then", () => ctx.go(`#/mission?at=${Math.round(L.time_ms)}`), `Mission as of ${fmtClock(L.time_ms)}`));
      out.push(linkBtn(I.map, "Map then", () => ctx.go(`#/map?at=${Math.round(L.time_ms)}`), `Map as of ${fmtClock(L.time_ms)}`));
    }
    return out.length ? h("div", { class: "tr-acts" }, out) : null;
  }

  function body(it, r) {
    if (it.kind === "commit") {
      const c = it.commit || {};
      return [
        h("div", { class: "tr-commit" }, h("code", { class: "tr-sha" }, c.short || shortSha(c.sha)),
          (c.branches || []).slice(0, 3).map((b) => h("span", { class: "tr-branch" }, b)),
          it.written ? h("span", { class: "tr-mut" }, `written ${fmtClock(it.written.ts_ms)} with ${it.written.tool}, committed ${fmtClock(it.ts_ms)}`) : c.author && !it.agent ? h("span", { class: "tr-mut" }, c.author) : null),
        diffLines(it.lines, r),
      ];
    }
    if (it.kind === "edit") return diffLines(it.lines, r);
    if (it.kind === "decision") {
      const c = it.ctx || {};
      return h("div", { class: "tr-dec" },
        (c.files || []).length ? h("div", { class: "tr-files" }, c.files.slice(0, 4).map((f) => h("code", { class: "lv-code" }, f))) : null,
        h("div", { class: "tr-dec-m" }, h("code", null, c.id), c.superseded ? pill("superseded since", "warn") : null,
          h("button", { class: "lv-link", type: "button", onclick: () => { location.hash = "#decisions"; }, title: "All of the repository's decisions" }, "Open in Decisions")));
    }
    if (it.kind === "question") {
      const q = it.question || {};
      return h("div", { class: "tr-q-b" },
        q.context ? h("p", { class: "tr-mut" }, q.context) : null,
        (q.options || []).length ? h("div", { class: "tr-opts" }, q.options.map((o) => {
          const chosen = o.label === q.answer;
          return h("div", { class: `tr-opt${chosen ? " on" : ""}` }, h("b", null, chosen ? "✓ " : "", o.label), o.detail ? h("span", null, o.detail) : null);
        })) : q.answer ? h("div", { class: "tr-opt on" }, h("b", null, "✓ ", q.answer)) : null);
    }
    if (it.kind === "rule") return it.rule?.ctx_id ? h("div", { class: "tr-mut" }, "kept in ChloeCtx as ", h("code", null, it.rule.ctx_id), it.rule.active === false ? " · off now" : "") : null;
    if (it.kind === "goal") {
      const g = it.goal || {};
      return g.outcome ? h("div", { class: "tr-goal" }, h("span", { class: "tr-mut" }, "outcome"), h("b", null, g.outcome.title), pill(String(g.outcome.state || "").replace("_", " "), g.outcome.state === "done" ? "good" : "mut")) : null;
    }
    return null;
  }

  function diffPanel(it) {
    const s = S.diffs.get(it.id);
    if (!s) return null;
    if (s.loading) return h("div", { class: "tr-diff tr-mut" }, "Reading the commit…");
    if (s.err) return h("div", { class: "tr-diff bad" }, s.err);
    const r = S.data.reading;
    return h("div", { class: "tr-diff" }, (s.d.files || []).map((f) => h("div", { class: "tr-diff-f" },
      h("div", { class: "tr-lines-f" }, f.path, h("span", { class: "tr-mut" }, ` · ${f.status}`)),
      (f.hunks || []).map((hk) => h("div", { class: "tr-hunk" }, (hk.lines || []).map((l) =>
        h("div", { class: `tr-ln s${l.kind === "+" ? "p" : l.kind === "-" ? "m" : "c"}` }, h("span", { class: "tr-ln-n" }, l.new_line ?? ""), h("span", { class: "tr-ln-s" }, l.kind), h("code", null, marked(l.text, r)))))))));
  }

  function whyPanel(key) {
    const s = S.whys.get(key);
    if (!s) return null;
    if (s.loading) return h("div", { class: "tr-why tr-mut" }, "Tracing the line through ChloeCtx and Workbench…");
    if (s.err) return h("div", { class: "tr-why bad" }, s.err);
    const d = s.d;
    const a = d.who?.agent;
    const notes = d.why || [];
    const c = d.where?.commit;
    // "Wrote it" only on the evidence of an edit; else the commit says it.
    return h("div", { class: "tr-why" },
      h("div", { class: "tr-why-h" }, svg(I.why), h("b", null, `${d.line?.path}:${d.line?.line}`),
        d.at ? h("span", { class: "tr-mut" }, `as of ${shortSha(d.at)}`) : null,
        a && d.when ? [agentChip(a, ctx), d.when.edit_ts ? h("span", { class: "tr-mut" }, `wrote it ${when(toMs(d.when.edit_ts))}`) : null,
          d.who?.prompt ? h("span", { class: "tr-mut" }, `on ${d.who.prompt.by === "you" ? "your" : d.who.prompt.by + "'s"} prompt`) : null] : null,
        !(a && d.when) && c ? h("span", { class: "tr-mut" }, `came in with ${shortSha(c.sha)} “${c.subject}” ${when(toMs(c.date))}${a ? `, on ${a.name}'s branch` : ""}`) : null),
      notes.length ? [d.why_from === "session" ? h("p", { class: "tr-mut" }, "Notes from that session around the commit (no edit record ties them to this line):") : null,
        h("ul", { class: "tr-why-n" }, notes.map((n) => h("li", null, ctxChip(n.kind), " ", n.text)))] : null,
      (d.missing || []).length ? h("p", { class: "tr-mut" }, d.missing[0]) : null,
      d.rewind ? linkBtn(I.play, "Rewind", () => ctx.openRewind(d.rewind)) : null);
  }

  function timeline() {
    const d = S.data;
    const r = d.reading || {};
    const kinds = S.kinds || new Set(KINDS);
    const all = d.timeline || [];
    const shown = all.filter((it) => kinds.has(ITEM[it.kind]?.group || "when"));
    if (!all.length) return h("div", { class: "tr-empty" }, "No commit, edit, decision or answer matches. ", (d.missing || []).length ? "See below what could not be searched." : "");
    if (!shown.length) return h("div", { class: "tr-empty" }, `${plural(all.length, "card")} hidden: turn on when or why above.`);
    let lastDay = null;
    return h("ol", { class: "tr-tl" }, shown.map((it) => {
      const n = all.indexOf(it) + 1;
      const day = new Date(it.ts_ms).toDateString();
      const dayRow = day !== lastDay ? h("li", { class: "tr-day" }, h("span", null, dayLabel(it.ts_ms))) : null;
      lastDay = day;
      const meta = ITEM[it.kind] || ITEM.commit;
      const isKey = it.key || d.answer?.key === it.id;
      return [dayRow, h("li", { class: `tr-it k-${it.kind}${isKey ? " key" : ""}`, "data-item": it.id },
        h("div", { class: "tr-when" }, h("b", null, fmtClock(it.ts_ms)), h("span", { class: "tr-e" }, `E${n}`)),
        h("div", { class: `tr-dot t-${meta.tone}` }, svg(I[it.kind] || I.commit)),
        h("article", { class: "tr-card" },
          h("header", { class: "tr-card-h" },
            it.kind === "decision" ? ctxChip("Decision", "A decision ChloeCtx kept from the agent's turn") : h("span", { class: `tr-kindlbl t-${meta.tone}` }, it.kind === "rule" ? "Rule you made" : meta.label),
            agentOf(it), changePill(it, r),
            isKey ? h("span", { class: "tr-keytag" }, "the change") : null),
          h("div", { class: `tr-title${it.kind === "decision" ? " quote" : ""}` }, it.kind === "decision" || it.kind === "rule" ? `“${it.title}”` : marked(it.title, r)),
          it.text ? h("p", { class: "tr-text" }, marked(it.text, r)) : null,
          body(it, r),
          actions(it),
          diffPanel(it),
          whyPanel(it.id)))];
    }));
  }

  function dayLabel(ms) {
    const day = fmtDay(ms);
    const date = new Date(ms).toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" });
    return day === "today" || day === "yesterday" ? `${day[0].toUpperCase()}${day.slice(1)} · ${date}` : date;
  }

  function nowPanel() {
    const d = S.data;
    const now = d.now || {};
    const r = d.reading || {};
    const how = !S.kinds || S.kinds.has("how");
    const src = now.source === "code_chunks"
      ? ["BM25 over code_chunks", now.pack ? ` · PACK ${now.pack.budget.toLocaleString()} TOKENS, ${now.pack.used.toLocaleString()} used` : ""]
      : now.source ? [`git grep in ${now.code?.[0]?.tree === "main" || !now.code?.length ? "the main checkout" : now.code[0].tree + "'s worktree"}`] : [];
    const code = now.code || [];
    return h("section", { class: `lv-card tr-now${how ? "" : " folded"}` },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "How it works now"), h("span", { class: "tr-mut" }, src)),
      !how ? h("p", { class: "tr-mut" }, "Turn on how in the reading to see the code.")
        : code.length ? code.map((c, i) => {
          const lines = String(c.text || "").split("\n");
          const hits = new Set(c.hits || []);
          const key = `now:${i}`;
          return h("div", { class: "tr-code" },
            h("div", { class: "tr-code-h" },
              h("span", { class: "tr-path" }, c.path.includes("/") ? h("span", { class: "tr-mut" }, c.path.slice(0, c.path.lastIndexOf("/") + 1)) : null, h("b", null, base(c.path)), h("span", { class: "tr-mut" }, `:${c.start_line}–${c.end_line}`)),
              c.symbol ? h("code", { class: "tr-sym" }, c.symbol) : null,
              c.tree && c.tree !== "main" ? pill(`in ${c.tree}'s worktree`, "warn") : null,
              h("span", { class: "tr-grow" }),
              c.why ? h("button", { class: "lv-link", type: "button", onclick: () => why(c.why, key, lines[c.why.line - c.start_line]) }, "Why?") : null),
            h("pre", { class: "tr-pre" }, lines.map((t, j) => {
              const n = c.start_line + j;
              return h("div", { class: `tr-pl${hits.has(n) ? " hit" : ""}` }, h("span", { class: "tr-ln-n" }, n), h("code", null, marked(t, r)));
            })),
            whyPanel(key));
        }) : h("p", { class: "tr-mut" }, "No code matches the subject now.", (now.missing || []).length ? " " + now.missing.join(" ") : ""),
      how && (now.docs || []).length ? h("div", { class: "tr-docs" },
        h("h3", { class: "lv-h" }, "Docs"),
        now.docs.map((p) => h("a", { class: "tr-doc", href: p.url, target: "_blank", rel: "noopener noreferrer" },
          svg(I.doc), h("span", null, h("b", null, p.title), p.lib ? h("span", { class: "tr-mut" }, ` · ${p.lib}`) : null, h("span", { class: "tr-doc-x" }, p.excerpt)))))
        : null);
  }

  function thenPanel() {
    const d = S.data;
    const L = d.links || {};
    if (!L.time_ms) return null;
    const agentName = (id) => (d.timeline || []).find((t) => t.agent?.id === id)?.agent?.name || "the agent";
    return h("section", { class: "lv-card tr-then" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "The state then"), h("span", { class: "tr-mut" }, when(L.time_ms))),
      h("div", { class: "tr-then-l" },
        L.story ? linkBtn(I.book, `${agentName(L.story.agent)}'s story at ${fmtClock(L.story.at_ms)}`, () => ctx.go(`#/story/${L.story.agent}`)) : null,
        L.rewind ? linkBtn(I.play, `Rewind its terminal to ${fmtClock(L.rewind.at_ms || L.time_ms)}`, () => ctx.openRewind({ ...L.rewind, edit_ms: L.rewind.to_ms - 3000 })) : null,
        S.timeApi ? linkBtn(I.target, `Mission at ${fmtClock(L.time_ms)}`, () => ctx.go(`#/mission?at=${Math.round(L.time_ms)}`)) : null,
        S.timeApi ? linkBtn(I.map, `Map at ${fmtClock(L.time_ms)}`, () => ctx.go(`#/map?at=${Math.round(L.time_ms)}`)) : null,
        S.timeApi === false ? h("span", { class: "tr-mut" }, "Mission and Map as of then need time travel, which this build does not have.") : null));
  }

  function whoRow() {
    if (!S.kinds?.has("who")) return null;
    const seen = new Map();
    for (const it of S.data.timeline || []) {
      const k = it.agent ? it.agent.id : it.by || "?";
      const e = seen.get(k) || { a: it.agent, by: it.by, n: 0, kinds: new Set() };
      e.n++; e.kinds.add(it.kind);
      seen.set(k, e);
    }
    if (!seen.size) return null;
    return h("section", { class: "lv-card tr-who" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Who"), h("span", { class: "tr-mut" }, plural(seen.size, "hand"))),
      h("div", { class: "tr-who-l" }, [...seen.values()].map((e) => h("div", { class: "tr-who-i" },
        e.a ? agentChip(e.a, ctx) : h("span", { class: "tr-by" }, avatar(e.by === "you" ? "you" : { name: e.by || "?" }, 18), e.by || "?"),
        h("span", { class: "tr-mut" }, [...e.kinds].map((k) => (ITEM[k] || {}).label?.toLowerCase() || k).join(", "))))));
  }

  function missingCard() {
    const m = S.data?.missing || [];
    if (!m.length) return null;
    return h("section", { class: "tr-missing" }, svg(I.info), h("div", null, h("b", null, "Not found or not searched"), h("ul", null, m.map((x) => h("li", null, x)))));
  }

  function emptyState() {
    return h("div", { class: "tr-start" },
      h("p", { class: "tr-mut" }, "Name what changed in your words: quoted text, an identifier, a file, “from x to y”. Trace reads the question without a model, searches git on every branch (agents' wb/* too), Workbench's events and ChloeCtx, and lays out where it happened."),
      h("div", { class: "tr-ex" }, (S.examples || EXAMPLES).map((q) => h("button", { class: "tr-ex-b", type: "button", onclick: () => { S.q = q; S.over = null; run(); } }, svg(I.search), q))));
  }

  function loading() {
    return h("div", { class: "tr-loading" },
      h("div", { class: "tr-steps" }, h("span", { class: "tr-spin" }), "Reading the question · git log -G --all · Workbench's edits · ChloeCtx's decisions · the code now"),
      [0, 1, 2].map(() => h("div", { class: "tr-skel" }, h("i"), h("b"), h("b", { class: "s" }))));
  }

  function view() {
    const d = S.data;
    return [
      h("header", { class: "tr-head" },
        h("div", { class: "lv-head" }, h("h1", { class: "lv-h1" }, "Trace"),
          h("span", { class: "lv-sub" }, "when, why and how something changed: git on every branch, Workbench's events, ChloeCtx and your answers")),
        askBox(),
        readingRow()),
      !S.repo || (S.err && needsRepo(S.err)) ? (S.picker ||= repoPicker(ctx, { focus: false, why: "Trace reads a repository's git history on every branch, Workbench's events and ChloeCtx. No agent is needed: open the git checkout, then ask." })) : S.err ? h("div", { class: "tr-err" }, S.err) : null,
      !S.q.trim() && !d && !S.busy ? emptyState() : null,
      S.busy && !d ? loading() : null,
      d ? h("div", { class: `tr-body${S.busy ? " stale" : ""}` },
        answerCard(),
        h("div", { class: "tr-cols" },
          h("div", { class: "tr-left" },
            h("div", { class: "tr-tl-h" }, h("h2", { class: "lv-h2" }, "Where it happened"), h("span", { class: "tr-mut" }, plural((d.timeline || []).length, "card"), ", oldest first")),
            timeline(),
            missingCard()),
          h("div", { class: "tr-right" }, whoRow(), nowPanel(), thenPanel()))) : null,
    ];
  }

  function render() {
    if (!alive) return;
    const scroll = el.scrollTop;
    const focused = document.activeElement && el.contains(document.activeElement) && document.activeElement.classList.contains("tr-q");
    el.replaceChildren(...view().flat(Infinity).filter(Boolean));
    el.scrollTop = scroll;
    if (focused) { const q = el.querySelector(".tr-q"); q.focus(); q.setSelectionRange(q.value.length, q.value.length); }
  }

  dropEsc = onEscape(() => { if (S.editing) { S.editing = null; render(); } else if (S.explain) { S.explain = null; render(); } else dropEsc && dropEsc(); });
  render();
  if (S.q.trim()) run({ keepHash: true });
  else queueMicrotask(() => el.querySelector(".tr-q")?.focus());
  // Examples made of this repository's own files and commits.
  ctx.api(`/api/trace/examples${S.repo ? `?repo=${enc(S.repo)}` : ""}`).then((r) => { if (alive && r && (r.examples || []).length) { S.examples = r.examples; const ex = el.querySelector(".tr-ex"); if (ex) ex.replaceChildren(...S.examples.map((q) => h("button", { class: "tr-ex-b", type: "button", onclick: () => { S.q = q; S.over = null; run(); } }, svg(I.search), q))); } }).catch(() => {});

  return {
    update(state) { if (state && state.agents) setFleet(state.agents); },
    unmount() { alive = false; req++; dropEsc && dropEsc(); el.remove(); },
  };
}

useStyle("trace", `
.lv-screen.tr{padding:22px 28px 40px}
.tr .mono{font-family:var(--mono)}
.tr-i{width:15px;height:15px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round;flex:none}
.tr-mut{color:var(--muted)}
.tr-grow{flex:1}
.tr-head{display:flex;flex-direction:column;gap:14px;margin-bottom:16px}
.tr-ask{display:flex;align-items:center;gap:10px;background:var(--surface);border:1px solid var(--line-2);border-radius:14px;padding:6px 6px 6px 16px;box-shadow:var(--shadow-1)}
.tr-ask:focus-within{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.tr-ask-i{width:20px;height:20px;color:var(--muted)}
.tr-q{flex:1;min-width:0;border:0;background:none;color:var(--text);font:500 17px/1.4 var(--sans);padding:10px 0;outline:none}
.tr-q::placeholder{color:var(--muted);font-weight:400}
.tr-q::-webkit-search-cancel-button{display:none}
.tr-go{padding:9px 20px;font-size:14px;border-radius:10px}
.tr-reading{display:flex;align-items:center;flex-wrap:wrap;gap:7px}
.tr-lbl{font-size:11.5px;font-weight:650;color:var(--muted);text-transform:uppercase;letter-spacing:.06em;margin-right:3px}
.tr-kinds{display:inline-flex;background:var(--surface-3);border-radius:999px;padding:2px}
.tr-kind{border:0;background:none;color:var(--muted);font:600 12px var(--sans);padding:4px 11px;border-radius:999px;cursor:pointer}
.tr-kind[aria-pressed="true"]{background:var(--surface);color:var(--text);box-shadow:var(--shadow-1)}
.tr-sep{width:1px;height:18px;background:var(--line-2);margin:0 3px}
.tr-chip{display:inline-flex;align-items:center;border:1px solid var(--line-2);background:var(--surface);border-radius:8px;font-size:12.5px;overflow:hidden}
.tr-chip-b{display:inline-flex;align-items:center;gap:7px;border:0;background:none;color:var(--text);font:inherit;padding:4px 4px 4px 9px;cursor:text}
.tr-chip-b b{font-size:11px;font-weight:650;color:var(--muted)}
.tr-chip-b .mono{font-size:12px}
.tr-chip-del{border:0;background:none;color:var(--muted);padding:4px 7px 4px 3px;cursor:pointer;display:grid;opacity:.65}
.tr-chip:hover .tr-chip-del,.tr-chip-del:focus-visible{opacity:1;color:var(--text)}
.tr-chip-del .tr-i{width:12px;height:12px;stroke-width:2.2}
.tr-chip.tr-c-x{border-color:color-mix(in srgb,var(--crit) 45%,var(--line-2));background:color-mix(in srgb,var(--crit) 7%,var(--surface))}
.tr-chip.tr-c-y{border-color:color-mix(in srgb,var(--good) 50%,var(--line-2));background:color-mix(in srgb,var(--good) 8%,var(--surface))}
.tr-chip.editing{padding:2px 4px 2px 9px;gap:6px;border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.tr-chip.editing b{font-size:11px;color:var(--muted)}
.tr-chip-in{border:0;background:none;color:var(--text);font:12.5px var(--mono);width:150px;outline:none;padding:3px 2px}
.tr-chip-in.wide{width:300px;font-family:var(--sans)}
.tr-arrow{color:var(--muted);font-weight:600}
.tr-add{display:inline-flex;align-items:center;gap:5px;border:1px dashed var(--line-2);background:none;color:var(--muted);border-radius:8px;padding:4px 10px;font:12.5px var(--sans);cursor:pointer}
.tr-add:hover{color:var(--text);border-color:var(--muted)}.tr-add .tr-i{width:13px;height:13px}
.tr-searched{margin-left:auto;color:var(--muted);font-size:12px;display:inline-flex;gap:5px;align-items:center;flex-wrap:wrap}
.tr-m{background:none;color:inherit;border-radius:4px;padding:0 2px;font-weight:650}
.tr-m-x{background:color-mix(in srgb,var(--crit) 16%,transparent);color:var(--crit-ink)}
.tr-m-y{background:color-mix(in srgb,var(--good) 18%,transparent);color:var(--good-ink)}
.tr-m-s{background:color-mix(in srgb,var(--accent) 16%,transparent);color:var(--accent)}
.tr-err,.tr-explained.bad,.tr-diff.bad,.tr-why.bad{color:var(--crit-ink);background:var(--crit-soft);border-radius:10px;padding:10px 14px}
.tr-body.stale{opacity:.55;transition:opacity .2s}
.tr-answer{background:var(--surface);border:1px solid var(--line);border-left:4px solid var(--accent);border-radius:12px;padding:16px 18px 12px;box-shadow:var(--shadow-1);margin-bottom:18px}
.tr-answer.none{border-left-color:var(--line-2)}
.tr-history{margin:6px 0 0 34px;font-size:13px;color:var(--text-2);line-height:1.7}
.tr-hist{border:0;background:none;padding:0;font:inherit;color:inherit;cursor:pointer;border-bottom:1px dashed var(--line-2)}
.tr-hist:hover{color:var(--text);border-bottom-color:var(--accent)}
.tr-answer-top{display:flex;gap:12px;align-items:flex-start}
.tr-answer-i{width:30px;height:30px;border-radius:9px;background:var(--accent-soft);color:var(--accent);display:grid;place-items:center;flex:none;margin-top:1px}
.tr-answer-i .tr-i{width:17px;height:17px}
.tr-answer-t{margin:0;font-size:17px;line-height:1.5;font-weight:520;letter-spacing:-.005em;text-wrap:pretty}
.tr-answer-meta{display:flex;align-items:center;gap:14px;flex-wrap:wrap;margin:12px 0 0 42px;font-size:12.5px;color:var(--text-2)}
.tr-explain{display:inline-flex;align-items:center;gap:7px;padding:6px 12px;border-color:color-mix(in srgb,var(--ctx) 45%,var(--line-2));color:var(--ctx);font-weight:600}
.tr-explain:hover{background:color-mix(in srgb,var(--ctx) 9%,var(--surface))}
.tr-explain-n{font-weight:500;font-size:11px;color:var(--muted);border-left:1px solid var(--line-2);padding-left:7px}
.tr-explained{margin:12px 0 2px 42px;border:1px solid color-mix(in srgb,var(--ctx) 30%,var(--line));background:color-mix(in srgb,var(--ctx) 6%,var(--surface));border-radius:10px;padding:12px 14px}
.tr-explained p{margin:8px 0 0;font-size:14px;line-height:1.6}
.tr-explained-h{display:flex;gap:12px;align-items:center;font-size:12px}
.tr-explained.loading{display:flex;gap:10px;align-items:center;color:var(--text-2)}
.tr-cite{border:0;background:color-mix(in srgb,var(--ctx) 16%,transparent);color:var(--ctx);font:650 11px var(--mono);border-radius:5px;padding:1px 5px;margin:0 2px;cursor:pointer;vertical-align:1px}
.tr-cols{display:grid;grid-template-columns:minmax(0,1fr) minmax(360px,520px);gap:20px;align-items:start}
.tr-right{display:flex;flex-direction:column;gap:14px;position:sticky;top:0}
.tr-tl-h{display:flex;gap:10px;align-items:baseline;margin:2px 0 6px}
.tr-tl{list-style:none;margin:0;padding:0;position:relative}
.tr-tl::before{content:"";position:absolute;left:93px;top:8px;bottom:8px;width:2px;background:var(--line);border-radius:2px}
.tr-day{padding:14px 0 8px 110px;font-size:11.5px;font-weight:650;color:var(--muted);text-transform:uppercase;letter-spacing:.06em}
.tr-it{display:grid;grid-template-columns:72px 32px minmax(0,1fr);gap:0 6px;margin-bottom:12px;align-items:start}
.tr-when{text-align:right;padding-top:9px;display:flex;flex-direction:column;align-items:flex-end;gap:2px}
.tr-when b{font-size:12.5px;font-weight:600;font-variant-numeric:tabular-nums;white-space:nowrap}
.tr-e{font:600 10.5px var(--mono);color:var(--muted)}
.tr-dot{width:26px;height:26px;border-radius:50%;display:grid;place-items:center;margin:5px 0 0 3px;position:relative;z-index:1;background:var(--surface);border:2px solid currentColor}
.tr-dot .tr-i{width:13px;height:13px;stroke-width:2}
.t-accent{color:var(--accent)}.t-ctx{color:var(--ctx)}.t-good{color:var(--good)}.t-warn{color:var(--warn)}
.k-decision .tr-dot .tr-i{fill:currentColor}
.tr-card{background:var(--surface);border:1px solid var(--line);border-radius:12px;padding:11px 14px 10px;box-shadow:var(--shadow-1);min-width:0}
.tr-it.key .tr-card{border-color:color-mix(in srgb,var(--accent) 55%,var(--line));box-shadow:0 0 0 3px var(--accent-soft),var(--shadow-1)}
.tr-it.key .tr-dot{background:var(--accent);color:var(--accent)}
.tr-it.key .tr-dot .tr-i{color:var(--accent-ink)}
.tr-it.flash .tr-card{animation:trflash 1.4s ease-out}
@keyframes trflash{0%{box-shadow:0 0 0 6px var(--focus)}100%{box-shadow:0 0 0 0 transparent}}
.k-decision .tr-card{border-color:color-mix(in srgb,var(--ctx) 32%,var(--line));background:color-mix(in srgb,var(--ctx) 4%,var(--surface))}
.tr-card-h{display:flex;align-items:center;gap:10px;flex-wrap:wrap;font-size:12.5px;margin-bottom:5px}
.tr-kindlbl{font-size:11px;font-weight:700;text-transform:uppercase;letter-spacing:.06em}
.tr-by{display:inline-flex;align-items:center;gap:6px;color:var(--text-2)}
.tr-change{font-size:12px;font-family:var(--mono)}
.tr-keytag{margin-left:auto;font-size:11px;font-weight:700;color:var(--accent);background:var(--accent-soft);border-radius:999px;padding:2px 9px}
.tr-title{font-size:14px;font-weight:600;line-height:1.45}
.tr-title.quote{font-weight:560}
.k-decision .tr-title{color:var(--text)}
.tr-text{margin:3px 0 0;color:var(--text-2);font-size:13px}
.tr-commit{display:flex;align-items:center;gap:8px;flex-wrap:wrap;margin-top:7px;font-size:12px}
.tr-sha{font:600 12px var(--mono);background:var(--surface-3);border-radius:5px;padding:1px 6px}
.tr-branch{font:12px var(--mono);color:var(--text-2);border:1px solid var(--line-2);border-radius:5px;padding:0 6px}
.tr-lines,.tr-diff{margin-top:8px;border:1px solid var(--line);border-radius:8px;background:var(--sunken);overflow:hidden;font-size:12px}
.tr-lines-f{font:600 11.5px var(--mono);color:var(--text-2);padding:5px 10px;border-bottom:1px solid var(--line);background:var(--surface-2)}
.tr-lines-f:not(:first-child){border-top:1px solid var(--line)}
.tr-ln{display:grid;grid-template-columns:38px 14px minmax(0,1fr);align-items:baseline;padding:1px 0}
.tr-ln code{font:12px/1.55 var(--mono);white-space:pre;overflow:hidden;text-overflow:ellipsis}
.tr-ln-n{font:11px var(--mono);color:var(--muted);text-align:right;padding-right:6px;user-select:none}
.tr-ln-s{font:12px var(--mono);color:var(--muted)}
.tr-ln.sp{background:color-mix(in srgb,var(--good) 10%,transparent)}.tr-ln.sp .tr-ln-s{color:var(--good-ink)}
.tr-ln.sm{background:color-mix(in srgb,var(--crit) 9%,transparent)}.tr-ln.sm .tr-ln-s{color:var(--crit-ink)}
.tr-hunk+.tr-hunk{border-top:1px dashed var(--line)}
.tr-diff{padding:0}.tr-diff.tr-mut{padding:8px 10px}
.tr-acts{display:flex;flex-wrap:wrap;gap:4px 6px;margin-top:9px}
.tr-act{display:inline-flex;align-items:center;gap:5px;border:1px solid transparent;background:var(--surface-2);color:var(--text-2);border-radius:7px;padding:3px 9px;font:12px var(--sans);cursor:pointer}
.tr-act:hover{color:var(--text);border-color:var(--line-2);background:var(--surface)}
.tr-act .tr-i{width:13px;height:13px}
.tr-dec{margin-top:6px;display:flex;flex-direction:column;gap:6px}
.tr-files{display:flex;gap:6px;flex-wrap:wrap}
.tr-dec-m{display:flex;gap:10px;align-items:center;font-size:12px;color:var(--muted)}
.tr-dec-m code{font:11.5px var(--mono)}
.tr-q-b{margin-top:6px}.tr-q-b p{margin:0 0 7px;font-size:12.5px}
.tr-opts{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:6px}
.tr-opt{border:1px solid var(--line);border-radius:8px;padding:6px 10px;font-size:12.5px;display:flex;flex-direction:column;gap:1px;color:var(--muted)}
.tr-opt b{color:var(--text-2);font-weight:600}
.tr-opt.on{border-color:color-mix(in srgb,var(--good) 55%,var(--line));background:color-mix(in srgb,var(--good) 8%,transparent)}
.tr-opt.on b{color:var(--good-ink)}
.tr-goal{display:flex;gap:8px;align-items:center;margin-top:6px;font-size:12.5px;flex-wrap:wrap}
.tr-why{margin-top:8px;border:1px solid color-mix(in srgb,var(--ctx) 28%,var(--line));border-radius:9px;padding:9px 11px;font-size:12.5px;display:flex;flex-direction:column;gap:6px}
.tr-why-h{display:flex;gap:9px;align-items:center;flex-wrap:wrap}
.tr-why-n{margin:0;padding:0;list-style:none;display:flex;flex-direction:column;gap:4px}
.tr-why p{margin:0}
.tr-now .lv-card-head{flex-wrap:wrap;font-size:12px}
.tr-now.folded{opacity:.75}
.tr-code{border:1px solid var(--line);border-radius:10px;overflow:hidden;margin-bottom:10px}
.tr-code-h{display:flex;align-items:center;gap:8px;padding:7px 11px;background:var(--surface-2);border-bottom:1px solid var(--line);font-size:12.5px}
.tr-path{font:12px var(--mono)}
.tr-sym{font:11.5px var(--mono);color:var(--text-2);background:var(--surface-3);border-radius:5px;padding:0 6px}
.tr-pre{margin:0;padding:6px 0;background:var(--sunken);overflow-x:auto;max-height:300px;overflow-y:auto}
.tr-pl{display:grid;grid-template-columns:40px minmax(0,1fr);padding:0 10px 0 0}
.tr-pl code{font:12px/1.6 var(--mono);white-space:pre}
.tr-pl.hit{background:color-mix(in srgb,var(--accent) 11%,transparent);box-shadow:inset 3px 0 0 var(--accent)}
.tr-pl .tr-ln-n{padding-right:12px}
.tr-docs{margin-top:4px}.tr-docs .lv-h{margin:10px 0 6px}
.tr-doc{display:flex;gap:9px;padding:8px 10px;border-radius:9px;color:var(--text);text-decoration:none;font-size:12.5px;align-items:flex-start}
.tr-doc:hover{background:var(--surface-2)}
.tr-doc .tr-i{margin-top:2px;color:var(--muted)}
.tr-doc-x{display:block;color:var(--text-2);margin-top:2px;line-height:1.45}
.tr-then-l{display:flex;flex-wrap:wrap;gap:6px}
.tr-then .lv-card-head{font-size:12px}
.tr-who-l{display:flex;flex-direction:column;gap:7px}
.tr-who-i{display:flex;gap:10px;align-items:center;font-size:12.5px}
.tr-missing{display:flex;gap:10px;margin:14px 0 0 110px;padding:10px 14px;border-radius:10px;background:var(--surface-2);border:1px solid var(--line);font-size:12.5px;color:var(--text-2)}
.tr-missing ul{margin:4px 0 0;padding-left:18px}
.tr-missing .tr-i{margin-top:2px;color:var(--muted)}
.tr-empty{padding:28px;text-align:center;color:var(--muted);border:1px dashed var(--line-2);border-radius:12px}
.tr-start{max-width:860px}
.tr-start>p{margin:0 0 14px;line-height:1.55}
.tr-ex{display:flex;flex-direction:column;gap:6px}
.tr-ex-b{display:flex;gap:10px;align-items:center;text-align:left;border:1px solid var(--line);background:var(--surface);color:var(--text-2);border-radius:10px;padding:9px 13px;font:13.5px var(--sans);cursor:pointer}
.tr-ex-b:hover{color:var(--text);border-color:var(--line-2)}
.tr-loading{display:flex;flex-direction:column;gap:12px}
.tr-steps{display:flex;gap:10px;align-items:center;color:var(--text-2);font-size:13px}
.tr-spin{width:14px;height:14px;border-radius:50%;border:2px solid var(--line-2);border-top-color:var(--accent);animation:trspin .8s linear infinite;flex:none}
@keyframes trspin{to{transform:rotate(360deg)}}
.tr-skel{display:grid;grid-template-columns:26px 1fr;gap:6px 12px;padding:14px;border:1px solid var(--line);border-radius:12px;background:var(--surface)}
.tr-skel i{grid-row:span 2;width:26px;height:26px;border-radius:50%;background:var(--surface-3)}
.tr-skel b{height:11px;border-radius:6px;background:var(--surface-3)}.tr-skel b.s{width:55%}
@media (max-width:1100px){.tr-cols{grid-template-columns:1fr}.tr-right{position:static}}
@media (max-width:640px){.lv-screen.tr{padding:14px}.tr-it{grid-template-columns:52px 30px minmax(0,1fr)}.tr-tl::before{left:72px}.tr-day,.tr-missing{padding-left:0;margin-left:0}.tr-answer-meta,.tr-explained{margin-left:0}.tr-searched{margin-left:0}}
`);
