// Autocomplete and intent checks (SPEC.md, "Fifth build: Autocomplete").
//
// In an IDE for code, the unit of edit is code, and the editor completes
// identifiers and checks the syntax. Here the unit is the task (an intent
// given to an agent), so the box completes references into the project's
// memory (@agents, files, symbols, tests, ◆ decisions and rules, earlier
// tasks with how they ended) and its "syntax check" is an intent check: no
// done check, against a ◆ decision or rule, a guarded command, overlap with
// a running agent, a hotspot.
//
// Past the token (the bar of SPEC.md's "Fifth build"): an empty box
// proposes the three likeliest things to write now; a grey ghost finishes
// the whole sentence (Tab takes a word, Shift+Tab or → at the end takes it
// all), with the ◆ rule that applies and a done check from the tests that
// cover its files; every row says why it is offered; what each user picks,
// passes over and writes is recorded, and their boxes rank by it; after a
// pause a fast model may offer a better ghost (marked as the model's).
//
//   attachComplete(input, { field, ctx, repo?, agent?, check?, onGo?, user? }) -> { refresh(), detach() }
//
// `input` is an <input> or a <textarea>; `field` picks the sources and
// their order on the server (task, goal, steer, answer, trace, quick, sql,
// persona, why, change); `ctx` has `api` and `go` (the screens' ctx). Calling
// it again on the same element only updates its options, so a screen may
// call it on every render. The layers (the squiggles' mirror, the list,
// the hover card) live in <body> (or the element's <dialog>) and go away
// when the element leaves the page.
//
// GET /api/complete?field&q&cursor&repo&agent&user → {mode (propose |
// complete), token, suggestions: [{kind, sub?, label, insert, detail,
// because?, picked?, ref, replace: {start, end}, go?}], ghost: {text,
// parts, because, source} | null, context: {agent, now} | null, took_ms}
// POST /api/complete/events {user, field, repo, agent, shown, accepted, typed}
// POST /api/complete/ghost {text, field, repo, agent, user, ghost} → {available, ghost}
// POST /api/intent/check {text, repo, field, agent} → {marks: [{start,
// end, severity, kind, message, fix?, ref?}], refs: [{start, end, kind,
// label, detail}], took_ms}
import { h, useStyle, avatar, onEscape, later, repoOf, fmtAgo } from "./common.js";

/** The fields whose text is an intent, checked as it is typed. */
const CHECKED = new Set(["task", "goal", "steer", "answer", "persona", "change"]);
const KINDS = {
  agent: ["@", "agent"], file: ["▤", "file"], symbol: ["ƒ", "symbol"], decision: ["", "decision"], preference: ["", "preference"],
  fact: ["", "fact"], rule: ["", "rule"], task: ["↺", "earlier task"], goal: ["◎", "goal"], test: ["✓", "test"], table: ["T", "table"],
  column: ["c", "column"], keyword: ["kw", "SQL"], function: ["fn", "function"], persona: ["☺", "persona"], doc: ["❡", "docs"], trace: ["→", "Trace"],
};
const CTX_KINDS = new Set(["decision", "preference", "fact", "rule"]);
const MARK_TITLE = {
  no_done_check: "No done check", contradicts: "Goes against a decision", guarded: "Reaches a guarded command", overlap: "Overlaps a running agent",
  hotspot: "A hotspot", unknown_ref: "Unknown reference", superseded: "Cites a superseded decision", read_only: "Asks for no changes",
};
const TRACE_RE = /^\s*(when|why|how|who)\b|\bchang/i;
/** The boxes a ghost sentence is offered in. */
const GHOSTED = new Set(["task", "goal", "steer", "answer", "persona", "change", "trace", "quick", "why"]);
/** A proposal's chip, by what it comes from. */
const SUBS = {
  test: ["✓", "a failing test"], error: ["!", "a runtime error"], hotspot: ["▲", "a hotspot"], outcome: ["◎", "the goal's next step"], mine: ["↺", "yours again"],
  said: ["❝", "you said before"], rule: ["", "◆ applies"], done: ["✓", "done check"], question: ["?", "today"],
};
Object.assign(KINDS, { mine: ["↺", "yours"], prompt: ["❝", "your prompt"], proposal: ["✦", "suggested"] });
/** The model ghost is off for the page once the server says it cannot run. */
let modelOff = false;
/** Who types: an id this browser keeps, so each person's habits are theirs. */
function userId() {
  try {
    let u = localStorage.getItem("wb.complete.user");
    if (!u) { u = "u-" + Math.random().toString(36).slice(2, 10); localStorage.setItem("wb.complete.user", u); }
    return u;
  } catch { return "you"; }
}

const insts = new WeakMap();
const live = new Set();
let ticker = null;

export function attachComplete(input, opts = {}) {
  if (!input) return null;
  const had = insts.get(input);
  if (had) { had.set(opts); return had.api; }
  const inst = make(input, opts);
  insts.set(input, inst);
  live.add(inst);
  if (!ticker) ticker = setInterval(tick, 400);
  return inst.api;
}

/** Repositions every layer, and drops those whose element left the page. */
function tick() {
  for (const inst of live) {
    if (!inst.input.isConnected) { inst.destroy(); continue; }
    inst.place();
  }
  if (!live.size) { clearInterval(ticker); ticker = null; }
}
addEventListener("scroll", () => { for (const i of live) i.place(); }, true);
addEventListener("resize", () => { for (const i of live) i.place(); });

/** The word at the cursor as the server reads it (a trigger and what follows). */
export function tokenAt(text, cursor) {
  const before = text.slice(0, cursor);
  const m = before.match(/[@◆#]?[\p{L}\p{N}_./:\-]*$/u);
  return { text: m ? m[0] : "", start: cursor - (m ? m[0].length : 0), end: cursor };
}

/** Moves marks over an edit: those before it stay, those after shift, those it touched go. */
export function shiftMarks(marks, before, after) {
  let p = 0;
  while (p < before.length && p < after.length && before[p] === after[p]) p++;
  let s = 0;
  while (s < before.length - p && s < after.length - p && before[before.length - 1 - s] === after[after.length - 1 - s]) s++;
  const oldEnd = before.length - s, d = after.length - before.length;
  return marks.filter((m) => m.end <= p || m.start >= oldEnd).map((m) => (m.start >= oldEnd ? { ...m, start: m.start + d, end: m.end + d } : m));
}

function make(input, opts0) {
  let O = { ...opts0 };
  const S = { items: [], sel: 0, open: false, token: null, marks: [], refs: [], checked: "", took: null, req: 0, creq: 0, skip: false, hover: -1, cardFor: null, offEsc: null, missing: [],
    mode: "complete", context: null, ghost: null, ghostAt: "", greq: 0, typedSent: "" };
  const isArea = input.tagName === "TEXTAREA";
  const field = () => O.field || "task";
  const repo = () => (typeof O.repo === "function" ? O.repo() : O.repo) || (O.ctx && O.ctx.params && O.ctx.params.repo) || (O.ctx && typeof O.ctx.repo === "function" && O.ctx.repo()) || repoOf(O.ctx) || "";
  const agent = () => (typeof O.agent === "function" ? O.agent() : O.agent) || "";
  const api = (p, o) => (O.ctx && O.ctx.api ? O.ctx.api(p, o) : Promise.reject(new Error("no api")));
  const checking = () => O.check !== false && CHECKED.has(field());
  const user = () => O.user || (O.ctx && O.ctx.user) || userId();
  /** Records what the box showed, which was picked, what was written. */
  function note(body) {
    if (!repo() && !agent()) return;
    api("/api/complete/events", { method: "POST", body: { user: user(), field: field(), repo: repo() || undefined, agent: agent() || undefined, ...body } }).catch(() => {});
  }
  const shownOf = (items) => items.slice(0, 12).map((x, i) => ({ kind: evKind(x), label: x.label, rank: i, source: x.source || "chloedb" }));
  const evKind = (x) => (x.kind === "proposal" && x.sub ? `proposal:${x.sub}` : x.kind);

  // ------------------------------------------------------------- layers
  let layers = null;
  function ensure() {
    if (layers || !input.isConnected) return layers;
    const host = input.closest("dialog") || document.body;
    const mirror = h("div", { class: "cx-mirror", "aria-hidden": "true" });
    const inner = h("div", { class: "cx-mi" });
    mirror.append(inner);
    const meas = h("div", { class: "cx-mirror cx-meas", "aria-hidden": "true" });
    const dd = h("div", { class: "cx-dd", role: "listbox", hidden: true, onmousedown: (e) => e.preventDefault() });
    const card = h("div", { class: "cx-card", role: "tooltip", hidden: true, onmouseenter: () => { clearTimeout(S.hideT); }, onmouseleave: () => hideCardSoon() });
    const badge = h("button", { class: "cx-badge", type: "button", hidden: true, tabindex: "-1", onmousedown: (e) => e.preventDefault(), onclick: () => showCard(-1, badge.getBoundingClientRect()) });
    host.append(mirror, meas, dd, card, badge);
    layers = { host, mirror, inner, meas, dd, card, badge };
    copyStyle();
    new ResizeObserver(() => place()).observe(input);
    return layers;
  }
  const PROPS = ["fontFamily", "fontSize", "fontWeight", "fontStyle", "letterSpacing", "lineHeight", "textTransform", "wordSpacing", "tabSize", "textIndent",
    "paddingTop", "paddingRight", "paddingBottom", "paddingLeft", "borderTopWidth", "borderRightWidth", "borderBottomWidth", "borderLeftWidth", "boxSizing", "textAlign"];
  function copyStyle() {
    if (!layers) return;
    const cs = getComputedStyle(input);
    for (const el of [layers.mirror, layers.meas]) {
      for (const p of PROPS) el.style[p] = cs[p];
      el.style.borderStyle = "solid";
      el.style.borderColor = "transparent";
      el.style.whiteSpace = isArea ? "pre-wrap" : "pre";
      el.style.overflowWrap = isArea ? "break-word" : "normal";
      el.classList.toggle("cx-line", !isArea);
    }
  }
  function visible() {
    const r = input.getBoundingClientRect();
    if (!r.width || !r.height) return false;
    for (const [x, y] of [[r.left + 6, r.top + Math.min(10, r.height / 2)], [r.left + r.width / 2, r.top + r.height / 2]]) {
      const at = document.elementFromPoint(x, y);
      if (at && (at === input || input.contains(at))) return true;
    }
    return false;
  }
  function place() {
    if (!layers) return;
    const r = input.getBoundingClientRect();
    const show = visible();
    const L = layers;
    for (const el of [L.mirror, L.meas]) {
      el.style.left = r.left + "px";
      el.style.top = r.top + "px";
      el.style.width = r.width + "px";
      el.style.height = r.height + "px";
    }
    // A textarea's scrollbar narrows its text: the mirror's too.
    const bl = parseFloat(getComputedStyle(input).borderLeftWidth) || 0, br = parseFloat(getComputedStyle(input).borderRightWidth) || 0;
    const sb = Math.max(0, input.offsetWidth - input.clientWidth - bl - br);
    const pr = parseFloat(getComputedStyle(input).paddingRight) || 0;
    L.mirror.style.paddingRight = L.meas.style.paddingRight = pr + sb + "px";
    L.inner.style.transform = `translate(${-input.scrollLeft}px, ${-input.scrollTop}px)`;
    L.mirror.hidden = !show || !(S.marks.length || S.refs.length || ghostShown());
    L.badge.hidden = !show || !S.marks.length;
    if (!L.badge.hidden) {
      const b = L.badge.getBoundingClientRect();
      L.badge.style.left = r.right - b.width - 6 + "px";
      L.badge.style.top = (isArea ? r.bottom - b.height - 6 : r.top + (r.height - b.height) / 2) + "px";
    }
    if (S.open) { if (!show) close(); else placeList(); }
    if (!show && !L.card.hidden) L.card.hidden = true;
  }

  // ------------------------------------------------------------- mirror
  function paint() {
    if (!ensure()) return;
    const text = input.value;
    const ranges = [
      ...S.refs.map((r, i) => ({ ...r, cls: `cx-ref cx-k-${r.kind}`, ri: i })),
      ...S.marks.map((m, i) => ({ ...m, cls: `cx-mark cx-${m.severity || "warning"}`, mi: i })),
    ].filter((x) => x.end > x.start && x.start < text.length + 1);
    const cuts = new Set([0, text.length]);
    for (const x of ranges) { cuts.add(Math.max(0, x.start)); cuts.add(Math.min(text.length, x.end)); }
    const at = [...cuts].sort((a, b) => a - b);
    const out = [];
    for (let i = 0; i < at.length - 1; i++) {
      const a = at[i], b = at[i + 1];
      const piece = text.slice(a, b);
      const on = ranges.filter((x) => x.start <= a && x.end >= b);
      if (!on.length) { out.push(piece); continue; }
      const mark = on.find((x) => x.mi != null);
      out.push(h("span", { class: on.map((x) => x.cls).join(" "), "data-m": mark ? String(mark.mi) : null }, piece));
    }
    if (ghostShown()) out.push(ghostSpan());
    out.push("​");
    layers.inner.replaceChildren(...out);
    const n = S.marks.length;
    const worst = S.marks.some((m) => m.severity === "error") ? "error" : S.marks.some((m) => m.severity === "warning") ? "warning" : "info";
    layers.badge.className = `cx-badge cx-${worst}`;
    layers.badge.replaceChildren(h("i"), String(n));
    layers.badge.title = S.marks.map((m) => MARK_TITLE[m.kind] || m.kind).join(" · ");
    place();
  }

  function caret() {
    const L = ensure();
    if (!L) return null;
    const pos = input.selectionStart ?? input.value.length;
    const mark = h("span", null, "​");
    L.meas.replaceChildren(input.value.slice(0, pos), mark);
    L.meas.scrollTop = 0;
    const r = mark.getBoundingClientRect();
    return { x: r.left - input.scrollLeft, top: r.top - input.scrollTop, bottom: r.bottom - input.scrollTop };
  }

  // ------------------------------------------------------------- the list
  async function complete(force = false) {
    const text = input.value;
    const cursor = input.selectionStart ?? text.length;
    const tok = tokenAt(text, cursor);
    const quick = field() === "quick";
    const empty = !text.trim();
    const atEnd = cursor >= text.length && input.selectionEnd === input.selectionStart;
    const wantsGhost = GHOSTED.has(field()) && atEnd && !empty;
    if (!force && !empty && !tok.text && !wantsGhost && !(field() === "sql" && /(\bfrom|\bjoin|\.)\s*$/i.test(text.slice(0, cursor))) && !(quick && TRACE_RE.test(text))) { close(); setGhost(null); return; }
    if (empty && field() === "sql") { close(); return; }
    // Words to an agent: the list opens on `@` or a path-like word only
    // (Maya: it opened over every sentence she typed to an agent).
    if (!force && field() === "steer" && !/^@|\/|\.[A-Za-z0-9]{1,6}$/.test(tok.text || "")) { close(); setGhost(null); return; }
    const r = ++S.req;
    let d;
    try {
      d = await api(`/api/complete?${new URLSearchParams({ field: field(), q: text, cursor: String(cursor), repo: repo(), agent: agent(), user: user(), ...(force ? { force: "1" } : {}) })}`);
    } catch (e) {
      d = { suggestions: [], missing: [e.message || String(e)] };
    }
    if (r !== S.req || document.activeElement !== input) return;
    let items = d.suggestions || [];
    // Quick search hands a question to Trace, first.
    if (quick && TRACE_RE.test(text) && text.trim().split(/\s+/).length >= 2 && !items.some((x) => x.kind === "trace")) {
      items = [{ kind: "trace", label: `Trace: ${text.trim()}`, detail: "when, why and how it changed, with the evidence", go: `#/trace?q=${encodeURIComponent(text.trim())}` }, ...items];
    }
    // Proposals the user typed over were passed over.
    if (S.mode === "propose" && S.open && S.items.length && !empty) note({ shown: shownOf(S.items), accepted: null });
    S.items = items.slice(0, 12);
    S.token = d.token || tok;
    S.took = d.took_ms;
    S.missing = d.missing || [];
    S.mode = d.mode || "complete";
    S.context = d.context || null;
    S.sel = 0;
    setGhost(d.ghost && d.ghost.text ? { ...d.ghost, at: text } : null);
    // After a space the ghost speaks; the list waits for a word.
    const list = S.items.length && (S.mode === "propose" || tok.text || force || !S.ghost || (quick && S.items[0].kind === "trace"));
    if (list) openList(); else close();
    if (wantsGhost) modelSoon();
  }
  const completeSoon = later(() => complete(false), 70);

  // ------------------------------------------------------------- the ghost
  function ghostShown() {
    const g = S.ghost;
    return !!(g && g.text && g.at === input.value && document.activeElement === input && (input.selectionStart ?? 0) >= input.value.length);
  }
  function setGhost(g) {
    S.ghost = g;
    if (layers || g) paint();
  }
  function ghostSpan() {
    const g = S.ghost;
    const parts = g.parts && g.parts.length && g.parts.map((p) => p.text).join("") === g.text ? g.parts : [{ kind: "text", text: g.text }];
    return h("span", { class: `cx-ghost${g.source === "model" ? " cx-g-model" : ""}` },
      ...parts.map((p) => h("span", { class: `cx-gp cx-gp-${p.kind}` }, p.text)),
      h("span", { class: "cx-gsrc" }, h("kbd", null, "Tab"), g.source === "model" ? ` ${g.model || "model"}` : " ChloeDB"));
  }
  /** Takes the next word of the ghost, or all of it. */
  function takeGhost(all) {
    const g = S.ghost;
    if (!g) return false;
    const m = all ? [g.text] : g.text.match(/^\s*[^\s]+/);
    const piece = m ? m[0] : g.text;
    const rest = g.text.slice(piece.length);
    input.focus();
    input.setRangeText(piece, input.value.length, input.value.length, "end");
    const now = input.value;
    S.skip = true;
    input.dispatchEvent(new Event("input", { bubbles: true }));
    S.skip = false;
    if (rest) {
      const parts = (g.parts || []).length ? trimParts(g.parts, piece.length) : null;
      setGhost({ ...g, text: rest, parts, at: now });
    } else {
      note({ shown: [{ kind: "ghost", label: g.at + g.text, rank: 0, source: g.source || "chloedb" }], accepted: { kind: "ghost", label: g.at + g.text, rank: 0, source: g.source || "chloedb" } });
      setGhost(null);
      completeSoon();
    }
    checkSoon();
    return true;
  }
  function trimParts(parts, n) {
    const out = [];
    for (const p of parts) {
      if (n >= p.text.length) { n -= p.text.length; continue; }
      out.push({ ...p, text: p.text.slice(n) });
      n = 0;
    }
    return out;
  }
  // After a pause, a fast model may offer a better ghost; it counts only
  // when it comes before the next keystroke.
  const modelSoon = later(async () => {
    if (modelOff || O.model === false) return;
    const text = input.value;
    if (text.trim().length < 6 || document.activeElement !== input || (input.selectionStart ?? 0) < text.length) return;
    const r = ++S.greq;
    let d;
    try { d = await api("/api/complete/ghost", { method: "POST", body: { text, field: field(), repo: repo() || undefined, agent: agent() || undefined, user: user(), ghost: S.ghost && S.ghost.at === text ? S.ghost.text : undefined } }); } catch { return; }
    if (d && d.available === false) { modelOff = true; return; }
    if (r !== S.greq || input.value !== text || !d || !d.ghost || !d.ghost.text) return;
    // Grounded, or not offered (SPEC.md, "Sixth build: E"): a model's ghost
    // becomes the one-key default only when the server checked its names.
    if (d.ghost.source === "model" && d.ghost.checked !== true) return;
    setGhost({ ...d.ghost, at: text, parts: null });
  }, 700);

  function openList() {
    const L = ensure();
    if (!L) return;
    if (!S.open) { S.open = true; S.offEsc = onEscape(close); }
    renderList();
    L.dd.hidden = false;
    placeList();
  }
  function close() {
    completeSoon.cancel();
    S.open = false;
    if (S.offEsc) { S.offEsc(); S.offEsc = null; }
    if (layers) layers.dd.hidden = true;
  }
  function placeList() {
    const c = caret();
    if (!c) return;
    const dd = layers.dd;
    const r = dd.getBoundingClientRect();
    const ir = input.getBoundingClientRect();
    // A search box's list hangs from the box; a text's from the caret.
    // Proposals hang from the box too: the caret of an empty box is its corner.
    const wide = ["quick", "trace", "search"].includes(field()) || S.mode === "propose";
    dd.style.width = wide ? Math.max(S.mode === "propose" ? Math.min(600, innerWidth - 16) : 0, Math.min(760, ir.width)) + "px" : "";
    const w = dd.getBoundingClientRect().width;
    const left = wide ? Math.max(8, Math.min(ir.left, innerWidth - w - 8)) : Math.max(8, Math.min(innerWidth - r.width - 8, Math.max(ir.left, c.x - 14)));
    const below = wide ? Math.max(c.bottom + 6, ir.bottom + 4) : c.bottom + 6;
    const top = below + r.height > innerHeight - 8 && c.top - r.height - 6 > 8 ? c.top - r.height - 6 : below;
    dd.style.left = left + "px";
    dd.style.top = top + "px";
  }

  function hl(label, tokText) {
    const t = String(tokText || "").replace(/^[@◆#]/, "").toLowerCase();
    if (!t) return label;
    const low = label.toLowerCase();
    let i = low.indexOf(t);
    // A word start first: "total" in "cart_total" rather than inside a word.
    const re = new RegExp(`(^|[^\\p{L}\\p{N}])${t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`, "u");
    const m = low.match(re);
    if (m) i = m.index + m[1].length;
    if (i < 0) return label;
    return [label.slice(0, i), h("b", null, label.slice(i, i + t.length)), label.slice(i + t.length)];
  }

  function chip(it) {
    if (it.kind === "proposal") {
      if (it.sub === "rule") return h("span", { class: "cx-chip cx-c-ctx" }, h("i"));
      return h("span", { class: `cx-chip cx-c-p cx-c-p-${it.sub}` }, (SUBS[it.sub] || ["✦"])[0]);
    }
    const [g] = KINDS[it.kind] || ["·"];
    if (it.kind === "agent") return h("span", { class: "cx-chip cx-c-agent" }, avatar(it.ref && it.ref.name ? { id: it.ref.id, name: it.ref.name } : { name: it.label.replace(/^@/, "") }, 18));
    if (CTX_KINDS.has(it.kind)) return h("span", { class: "cx-chip cx-c-ctx" }, h("i"));
    return h("span", { class: `cx-chip cx-c-${it.kind}` }, g);
  }

  function outcomePill(it) {
    const o = it.ref && it.ref.outcome;
    if (!o) return null;
    const tone = o === "done" ? "good" : o === "running" ? "acc" : o === "failed" || o === "stopped" ? "crit" : "warn";
    return h("span", { class: `cx-pill cx-p-${tone}` }, o === "done" ? "✓ done" : o.replace(/_/g, " "));
  }

  function renderList() {
    const dd = layers.dd;
    const tok = S.token && S.token.text;
    const rows = S.items.map((it, i) => {
      const ctxk = CTX_KINDS.has(it.kind) || (it.kind === "proposal" && it.sub === "rule");
      const label = it.kind === "file" ? pathLabel(it.label, tok) : it.kind === "trace" || it.kind === "proposal" ? codeLabel(it.label) : hl(it.label, tok);
      const kindLabel = it.kind === "proposal" ? (SUBS[it.sub] || [, "suggested"])[1] : ctxk ? `◆ ${it.kind}` : (KINDS[it.kind] || [, it.kind])[1];
      return h("div", { class: `cx-row${i === S.sel ? " sel" : ""}${ctxk ? " cx-ctxrow" : ""} cx-r-${it.kind}`, role: "option", "aria-selected": String(i === S.sel),
        onmousemove: () => { if (S.sel !== i) { S.sel = i; renderList(); } }, onclick: () => accept(i) },
        chip(it),
        h("div", { class: "cx-main" },
          h("div", { class: "cx-l" }, h("span", { class: "cx-lt" }, label), outcomePill(it)),
          it.detail && it.kind !== "proposal" && it.detail !== it.because ? h("div", { class: "cx-d" }, it.detail) : null,
          it.because ? h("div", { class: "cx-why" }, h("span", null, "because"), " ", it.because) : null),
        h("span", { class: "cx-side" },
          it.picked ? h("span", { class: "cx-picked", title: `you picked it ${it.picked} time${it.picked === 1 ? "" : "s"}` }, `★ ${it.picked}`) : null,
          h("span", { class: `cx-kind${ctxk ? " ctx" : ""}` }, kindLabel)));
    });
    const head = S.mode === "propose"
      ? h("div", { class: "cx-head" }, h("span", { class: "cx-hk" }, "Suggested now"),
        S.context && S.context.agent ? h("span", { class: "cx-hc" }, h("b", null, S.context.agent), " · ", S.context.now || "") : h("span", { class: "cx-hc" }, "from what is happening in this repository"))
      : null;
    dd.classList.toggle("cx-propose", S.mode === "propose");
    dd.replaceChildren(...[
      head,
      h("div", { class: "cx-list" }, rows),
      h("div", { class: "cx-foot" }, h("span", null, h("kbd", null, "↑↓")), h("span", null, h("kbd", null, "Tab"), " accept"), h("span", null, h("kbd", null, "Esc")),
        h("span", { class: "cx-src" }, S.missing.length ? S.missing[0] : `ChloeDB${S.took != null ? ` · ${S.took} ms` : ""}`)),
    ].filter(Boolean));
    dd.querySelector(".cx-row.sel")?.scrollIntoView({ block: "nearest" });
  }

  /** A sentence with its `code` spans set in mono. */
  function codeLabel(t) {
    const out = [];
    String(t).split(/(`[^`]+`)/).forEach((x, i) => out.push(i % 2 ? h("code", { class: "cx-code" }, x.slice(1, -1)) : x));
    return out;
  }

  function pathLabel(p, tok) {
    const i = p.lastIndexOf("/");
    return h("span", { class: "cx-path" }, i >= 0 ? h("span", { class: "cx-dir" }, p.slice(0, i + 1)) : null, h("span", { class: "cx-base" }, hl(p.slice(i + 1), tok)));
  }

  function accept(i) {
    const it = S.items[i];
    if (!it) return;
    note({ shown: shownOf(S.items), accepted: { kind: evKind(it), label: it.label, rank: i, source: it.source || "chloedb" } });
    close();
    if (it.go) {
      const go = O.onGo || (O.ctx && O.ctx.go);
      if (go) go(it.go);
      return;
    }
    const rep = it.replace || S.token || tokenAt(input.value, input.selectionStart ?? input.value.length);
    const ins = it.insert ?? it.label;
    input.focus();
    input.setRangeText(ins, rep.start, rep.end, "end");
    S.skip = true;
    input.dispatchEvent(new Event("input", { bubbles: true }));
    S.skip = false;
    checkSoon();
    // The rest of the sentence, now the word is in.
    if (GHOSTED.has(field())) completeSoon();
  }

  // ------------------------------------------------------------- the check
  async function check() {
    if (!checking()) { S.marks = []; S.refs = []; paint(); return; }
    const text = input.value;
    if (!text.trim()) { S.marks = []; S.refs = []; S.checked = text; paint(); return; }
    if (text === S.checked) return;
    const r = ++S.creq;
    let d;
    try { d = await api("/api/intent/check", { method: "POST", body: { text, repo: repo(), field: field(), agent: agent() || undefined, ...(O.checkExtra ? O.checkExtra() : {}) } }); } catch { return; }
    if (r !== S.creq || input.value !== text) return;
    S.checked = text;
    S.marks = d.marks || [];
    S.refs = d.refs || [];
    paint();
  }
  const checkSoon = later(check, 380);

  // ------------------------------------------------------------- the card
  function hideCardSoon() { clearTimeout(S.hideT); S.hideT = setTimeout(() => { if (layers) layers.card.hidden = true; S.cardFor = null; }, 220); }
  function showCard(mi, rect) {
    const L = ensure();
    if (!L) return;
    clearTimeout(S.hideT);
    const marks = mi < 0 ? S.marks : [S.marks[mi]].filter(Boolean);
    if (!marks.length) return;
    S.cardFor = mi;
    L.card.replaceChildren(...marks.map((m) => markBody(m)));
    L.card.hidden = false;
    const c = L.card.getBoundingClientRect();
    const left = Math.max(8, Math.min(innerWidth - c.width - 8, rect.left - 10));
    const top = rect.top - c.height - 8 > 8 ? rect.top - c.height - 8 : rect.bottom + 8;
    L.card.style.left = left + "px";
    L.card.style.top = top + "px";
  }
  function markBody(m) {
    const ref = m.ref || null;
    const fix = m.fix || null;
    const sev = m.severity || "warning";
    return h("div", { class: `cx-cb cx-${sev}` },
      h("div", { class: "cx-ch" }, h("span", { class: `cx-sev cx-${sev}` }, sev === "error" ? "!" : sev === "info" ? "i" : "▲"), h("b", null, MARK_TITLE[m.kind] || m.kind),
        h("span", { class: "cx-span" }, `“${input.value.slice(m.start, m.end).trim().slice(0, 40)}${m.end - m.start > 40 ? "…" : ""}”`),
        ref && CTX_KINDS.has(ref.kind) ? h("span", { class: "lv-ctx cx-from" }, "ChloeCtx") : null),
      h("p", { class: "cx-msg" }, m.message),
      ref && ref.text ? h("div", { class: `cx-refcard${CTX_KINDS.has(ref.kind) ? " ctx" : ""}` },
        CTX_KINDS.has(ref.kind) ? h("span", { class: "cx-dia" }) : null,
        h("div", null, h("div", { class: "cx-rt" }, ref.text),
          h("div", { class: "cx-rm" }, [ref.kind, ref.by, ref.ts ? fmtAgo(ref.ts) : null, ref.id].filter(Boolean).join(" · ")))) : null,
      fix || m.go ? h("div", { class: "cx-fixes" },
        fix ? h("button", { class: "cx-fix", type: "button", onmousedown: (e) => e.preventDefault(), onclick: () => applyFix(fix) }, h("span", { class: "cx-wand" }, "✦"), fix.label) : null,
        m.go ? h("button", { class: "cx-alt", type: "button", onmousedown: (e) => e.preventDefault(), onclick: () => { layers.card.hidden = true; const go = O.onGo || (O.ctx && O.ctx.go); if (go) go(m.go.hash || m.go); } }, m.go.label || "Open") : null) : null);
  }
  function applyFix(fix) {
    if (layers) layers.card.hidden = true;
    if (fix.go) { const go = O.onGo || (O.ctx && O.ctx.go); if (go) go(fix.go); return; }
    // A fix that sets a field of the form ("Start it read-only"), not the text.
    if (fix.set) { if (O.onSet) O.onSet(fix.set); S.checked = ""; check(); return; }
    const edits = [...(fix.edits || [])].sort((a, b) => b.start - a.start);
    if (!edits.length) return;
    input.focus();
    for (const e of edits) input.setRangeText(e.text, e.start, e.end, "end");
    S.skip = true;
    input.dispatchEvent(new Event("input", { bubbles: true }));
    S.skip = false;
    check();
  }
  function hoverAt(x, y) {
    if (!layers || layers.mirror.hidden) return;
    let hit = -1, rect = null;
    for (const sp of layers.inner.querySelectorAll("[data-m]")) {
      for (const r of sp.getClientRects()) {
        if (x >= r.left && x <= r.right && y >= r.top - 2 && y <= r.bottom + 3) { hit = Number(sp.dataset.m); rect = r; break; }
      }
      if (hit >= 0) break;
    }
    if (hit >= 0) { if (S.cardFor !== hit || layers.card.hidden) showCard(hit, rect); else clearTimeout(S.hideT); }
    else if (S.cardFor != null && S.cardFor >= 0) hideCardSoon();
  }

  // ------------------------------------------------------------- events
  let lastText = input.value;
  const onInput = () => {
    const now = input.value;
    if (S.marks.length || S.refs.length) {
      S.marks = shiftMarks(S.marks, lastText, now);
      S.refs = shiftMarks(S.refs, lastText, now);
      paint();
    }
    lastText = now;
    if (!S.skip) completeSoon();
    checkSoon();
  };
  const onKey = (e) => {
    if (e.key === " " && e.ctrlKey) { e.preventDefault(); e.stopImmediatePropagation(); complete(true); return; }
    if (e.isComposing) return;
    // The ghost: Tab a word (when no list is open), Shift+Tab or → all of it.
    if (ghostShown() && !e.metaKey && !e.ctrlKey && !e.altKey) {
      const all = (e.key === "Tab" && e.shiftKey) || e.key === "ArrowRight";
      if (all || (e.key === "Tab" && !S.open)) {
        e.preventDefault();
        e.stopImmediatePropagation();
        close();
        takeGhost(all);
        return;
      }
    }
    if (e.key === "Escape" && S.ghost && !S.open) { setGhost(null); }
    if (e.key === "Enter" && !S.open && !e.shiftKey && !isArea) sendTyped();
    if (!S.open) return;
    const n = S.items.length;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { S.sel = (S.sel + (e.key === "ArrowDown" ? 1 : -1) + n) % n; renderList(); }
    else if ((e.key === "Tab" && !e.shiftKey) || (e.key === "Enter" && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey)) accept(S.sel);
    else return;
    e.preventDefault();
    e.stopImmediatePropagation();
  };
  /** What the user wrote in the end: their words, for their own suggestions. */
  function sendTyped() {
    const t = input.value.trim();
    if (t.length < 4 || t === S.typedSent || field() === "sql") return;
    S.typedSent = t;
    note({ typed: t });
  }
  const onBlur = () => setTimeout(() => { if (document.activeElement !== input) { close(); if (S.ghost) paint(); } }, 120);
  const onChange = () => sendTyped();
  const onMove = (e) => hoverAt(e.clientX, e.clientY);
  const onLeave = () => { if (S.cardFor != null && S.cardFor >= 0) hideCardSoon(); };
  const onScroll = () => place();
  const onFocus = () => { ensure(); copyStyle(); place(); if (!input.value.trim() && field() !== "sql") completeSoon(); };
  input.addEventListener("input", onInput);
  input.addEventListener("keydown", onKey, true);
  input.addEventListener("blur", onBlur);
  input.addEventListener("focus", onFocus);
  input.addEventListener("mousemove", onMove);
  input.addEventListener("mouseleave", onLeave);
  input.addEventListener("scroll", onScroll);
  input.addEventListener("change", onChange);
  input.addEventListener("click", () => { if (S.open) completeSoon(); if (S.ghost) paint(); });

  const inst = {
    input,
    place,
    set(o) { const f = field(); O = { ...O, ...o }; if (f !== field()) { S.checked = ""; checkSoon(); } },
    destroy() {
      close();
      input.removeEventListener("input", onInput);
      input.removeEventListener("keydown", onKey, true);
      input.removeEventListener("blur", onBlur);
      input.removeEventListener("focus", onFocus);
      input.removeEventListener("mousemove", onMove);
      input.removeEventListener("mouseleave", onLeave);
      input.removeEventListener("scroll", onScroll);
      input.removeEventListener("change", onChange);
      if (layers) for (const k of ["mirror", "meas", "dd", "card", "badge"]) layers[k].remove();
      layers = null;
      live.delete(inst);
      insts.delete(input);
    },
    api: null,
  };
  inst.api = {
    refresh: () => { S.checked = ""; check(); },
    complete: () => complete(true),
    marks: () => S.marks,
    ghost: () => S.ghost,
    detach: () => inst.destroy(),
  };
  // A box that already holds text (a re-render, a draft) is checked at once;
  // an empty one that has the focus proposes.
  requestAnimationFrame(() => {
    if (input.value.trim() && checking()) check();
    if (!input.value.trim() && document.activeElement === input && field() !== "sql") complete(false);
  });
  return inst;
}

useStyle("complete", `
.cx-mirror{position:fixed;z-index:71;pointer-events:none;overflow:hidden;color:transparent;margin:0;background:none;box-shadow:none}
.cx-mirror.cx-line{display:flex;align-items:center}
.cx-mirror[hidden]{display:none}
.cx-meas{visibility:hidden;z-index:-1}
.cx-mi{min-width:100%}
.cx-mark{text-decoration-line:underline;text-decoration-style:wavy;text-decoration-thickness:1.5px;text-underline-offset:3px;text-decoration-skip-ink:none}
.cx-mark.cx-error{text-decoration-color:var(--crit)}
.cx-mark.cx-warning{text-decoration-color:var(--warn)}
.cx-mark.cx-info{text-decoration-color:var(--accent);text-decoration-style:dotted}
.cx-ref{border-radius:4px;box-shadow:0 0 0 1px color-mix(in srgb,currentColor 0%,transparent)}
.cx-k-agent{background:color-mix(in srgb,var(--accent) 16%,transparent)}
.cx-k-decision,.cx-k-rule,.cx-k-preference,.cx-k-fact{background:color-mix(in srgb,var(--ctx) 18%,transparent)}
.cx-k-file,.cx-k-symbol{background:color-mix(in srgb,var(--lane-1) 11%,transparent)}
.cx-k-test{background:color-mix(in srgb,var(--good) 14%,transparent)}
.cx-dd{position:fixed;z-index:76;width:min(520px,calc(100vw - 16px));background:var(--surface);color:var(--text);border:1px solid var(--line-2);border-radius:11px;box-shadow:var(--shadow-3);overflow:hidden;font:13px/1.4 var(--sans);text-align:left}
.cx-dd[hidden]{display:none}
.cx-list{max-height:min(360px,46vh);overflow:auto;padding:5px 0}
.cx-row{display:flex;align-items:center;gap:10px;padding:6px 12px 6px 10px;cursor:pointer;border-left:3px solid transparent}
.cx-row.sel{background:var(--accent-soft);border-left-color:var(--accent)}
.cx-row.cx-ctxrow.sel{background:color-mix(in srgb,var(--ctx) 12%,transparent);border-left-color:var(--ctx)}
.cx-chip{display:inline-grid;place-items:center;width:22px;height:22px;border-radius:6px;flex:none;font:700 11px var(--mono);background:var(--surface-3);color:var(--text-2)}
.cx-c-agent{background:none}.cx-c-agent .lv-av{border-radius:6px}
.cx-c-ctx{background:color-mix(in srgb,var(--ctx) 15%,transparent)}
.cx-c-ctx i{width:8px;height:8px;border-radius:2px;background:var(--ctx);transform:rotate(45deg)}
.cx-c-file{background:color-mix(in srgb,var(--lane-1) 15%,transparent);color:var(--lane-1);font-size:12px}
.cx-c-symbol{background:color-mix(in srgb,var(--lane-4) 15%,transparent);color:var(--lane-4);font-style:italic}
.cx-c-test{background:var(--good-soft);color:var(--good-ink)}
.cx-c-task,.cx-c-goal{background:color-mix(in srgb,var(--lane-2) 15%,transparent);color:var(--lane-2)}
.cx-c-table,.cx-c-column{background:color-mix(in srgb,var(--lane-3) 15%,transparent);color:var(--lane-3)}
.cx-c-keyword,.cx-c-function{font-size:9.5px}
.cx-c-trace{background:var(--accent);color:var(--accent-ink)}
.cx-c-persona,.cx-c-doc{background:color-mix(in srgb,var(--lane-5) 15%,transparent);color:var(--lane-5)}
.cx-main{flex:1;min-width:0;display:flex;flex-direction:column;gap:1px}
.cx-l{display:flex;align-items:center;gap:7px;min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;color:var(--text)}
.cx-l b{font-weight:750;color:var(--text)}
.cx-r-symbol .cx-l,.cx-r-test .cx-l,.cx-r-table .cx-l,.cx-r-column .cx-l,.cx-r-keyword .cx-l,.cx-r-function .cx-l,.cx-r-agent .cx-l{font-family:var(--mono);font-size:12.5px}
.cx-lt{min-width:0;overflow:hidden;text-overflow:ellipsis}
.cx-ctxrow .cx-l{white-space:normal}.cx-ctxrow .cx-lt{display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical}
.cx-r-trace .cx-l{font-weight:650;color:var(--accent)}
.cx-path{font:12.5px var(--mono);overflow:hidden;text-overflow:ellipsis}
.cx-dir{color:var(--muted)}.cx-base{color:var(--text);font-weight:600}
.cx-r-column .cx-main,.cx-r-table .cx-main,.cx-r-keyword .cx-main,.cx-r-function .cx-main{flex-direction:row;align-items:baseline;gap:10px}
.cx-r-column .cx-d,.cx-r-table .cx-d,.cx-r-keyword .cx-d,.cx-r-function .cx-d{flex:1}
.cx-d{font-size:11.5px;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.cx-kind{flex:none;font-size:10.5px;color:var(--muted);text-transform:lowercase}
.cx-kind.ctx{color:var(--ctx);font-weight:650}
.cx-pill{flex:none;border-radius:999px;padding:0 7px;font:600 10.5px/17px var(--sans)}
.cx-p-good{background:var(--good-soft);color:var(--good-ink)}.cx-p-warn{background:var(--warn-soft);color:var(--warn-ink)}
.cx-p-crit{background:var(--crit-soft);color:var(--crit-ink)}.cx-p-acc{background:var(--accent-soft);color:var(--accent)}
.cx-foot{display:flex;align-items:center;gap:12px;padding:6px 12px;border-top:1px solid var(--line);background:var(--surface-2);font-size:11px;color:var(--muted)}
.cx-foot kbd{font:10.5px var(--mono);background:var(--surface);border:1px solid var(--line-2);border-radius:4px;padding:0 4px;margin-right:3px}
.cx-src{margin-left:auto;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:60%}
.cx-badge{position:fixed;z-index:72;display:inline-flex;align-items:center;gap:4px;height:19px;padding:0 7px 0 5px;border-radius:999px;border:0;font:700 11px var(--sans);cursor:pointer}
.cx-badge[hidden]{display:none}
.cx-badge i{width:9px;height:9px;border-radius:50%;background:currentColor}
.cx-badge.cx-error{background:var(--crit-soft);color:var(--crit-ink)}.cx-badge.cx-warning{background:var(--warn-soft);color:var(--warn-ink)}.cx-badge.cx-info{background:var(--accent-soft);color:var(--accent)}
.cx-card{position:fixed;z-index:82;width:min(400px,calc(100vw - 16px));background:var(--surface);color:var(--text);border:1px solid var(--line-2);border-radius:11px;box-shadow:var(--shadow-3);font:12.5px/1.45 var(--sans);text-align:left;overflow:hidden}
.cx-card[hidden]{display:none}
.cx-cb{padding:11px 13px 12px;border-left:3px solid var(--warn)}
.cx-cb+.cx-cb{border-top:1px solid var(--line)}
.cx-cb.cx-error{border-left-color:var(--crit)}.cx-cb.cx-info{border-left-color:var(--accent)}
.cx-ch{display:flex;align-items:center;gap:7px;min-width:0}
.cx-ch b{font-size:13px;font-weight:700;white-space:nowrap}
.cx-sev{display:inline-grid;place-items:center;width:17px;height:17px;border-radius:5px;font:800 10px var(--sans);flex:none}
.cx-sev.cx-warning{background:var(--warn-soft);color:var(--warn-ink)}.cx-sev.cx-error{background:var(--crit-soft);color:var(--crit-ink)}.cx-sev.cx-info{background:var(--accent-soft);color:var(--accent)}
.cx-span{font:11.5px var(--mono);color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.cx-from{margin-left:auto;flex:none}
.cx-msg{margin:6px 0 0;color:var(--text-2)}
.cx-refcard{display:flex;gap:9px;margin-top:9px;padding:8px 10px;border-radius:8px;background:var(--surface-2);border:1px solid var(--line)}
.cx-refcard.ctx{background:color-mix(in srgb,var(--ctx) 8%,var(--surface));border-color:color-mix(in srgb,var(--ctx) 30%,var(--line))}
.cx-dia{width:8px;height:8px;margin-top:5px;border-radius:2px;background:var(--ctx);transform:rotate(45deg);flex:none}
.cx-rt{color:var(--text);font-weight:550}
.cx-rm{font-size:11px;color:var(--muted);margin-top:2px}
.cx-fixes{display:flex;flex-wrap:wrap;gap:6px;margin-top:10px}
.cx-fix,.cx-alt{display:inline-flex;align-items:center;gap:6px;border-radius:7px;padding:4px 10px;font:600 12px var(--sans);cursor:pointer}
.cx-fix{border:1px solid color-mix(in srgb,var(--accent) 40%,transparent);background:var(--accent-soft);color:var(--accent)}
.cx-fix:hover{background:var(--accent);color:var(--accent-ink)}
.cx-alt{border:1px solid var(--line-2);background:var(--surface);color:var(--text-2)}
.cx-alt:hover{color:var(--text);border-color:var(--text-2)}
.cx-wand{font-size:11px}
.cx-ghost{color:var(--muted);opacity:.78}
.cx-gp-rule{color:color-mix(in srgb,var(--ctx) 80%,var(--muted))}
.cx-gp-done{color:color-mix(in srgb,var(--good) 55%,var(--muted))}
.cx-g-model .cx-gp{color:color-mix(in srgb,var(--accent) 55%,var(--muted))}
.cx-gsrc{display:inline-block;margin-left:8px;padding:0 5px;border-radius:4px;border:1px solid var(--line-2);font:600 9.5px/15px var(--sans);color:var(--muted);vertical-align:1px;opacity:.9}
.cx-gsrc kbd{font:600 9.5px var(--mono);margin-right:2px}
.cx-why{font-size:11px;color:var(--text-2);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.cx-why span{color:var(--muted);font-style:italic}
.cx-side{flex:none;display:flex;flex-direction:column;align-items:flex-end;gap:2px}
.cx-picked{font:600 10px var(--sans);color:var(--warn-ink);background:var(--warn-soft);border-radius:999px;padding:0 6px;line-height:15px}
.cx-head{display:flex;align-items:baseline;gap:10px;padding:8px 12px 6px;border-bottom:1px solid var(--line);background:var(--surface-2);font-size:11.5px;color:var(--muted);min-width:0}
.cx-hk{flex:none;font-weight:750;letter-spacing:.06em;text-transform:uppercase;font-size:10.5px;color:var(--accent)}
.cx-hc{min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.cx-hc b{color:var(--text);font-weight:650}
.cx-propose{width:min(600px,calc(100vw - 16px))}
.cx-propose .cx-row{align-items:flex-start;padding-top:8px;padding-bottom:8px}
.cx-propose .cx-l{white-space:normal}
.cx-propose .cx-lt{display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;line-height:1.4}
.cx-code{font:11.5px var(--mono);background:var(--surface-3);border-radius:4px;padding:0 4px}
.cx-c-p{background:color-mix(in srgb,var(--accent) 14%,transparent);color:var(--accent)}
.cx-c-p-test,.cx-c-p-error{background:var(--crit-soft);color:var(--crit-ink)}
.cx-c-p-hotspot{background:var(--warn-soft);color:var(--warn-ink)}
.cx-c-p-outcome{background:color-mix(in srgb,var(--lane-2) 15%,transparent);color:var(--lane-2)}
.cx-c-p-said,.cx-c-p-mine,.cx-c-mine,.cx-c-prompt{background:color-mix(in srgb,var(--lane-5) 15%,transparent);color:var(--lane-5)}
.cx-c-p-done{background:var(--good-soft);color:var(--good-ink)}
`);
