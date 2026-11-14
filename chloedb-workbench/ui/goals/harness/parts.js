// Optimize the harness (HARNESS.md): the pieces its pages share, on the
// shared pieces of /levels/common.js and the theme's tokens only.
import { h, useStyle, icon, toMs } from "/levels/common.js";

/** The goal's tabs; `on` is the one shown. */
export const TABS = [
  ["overview", "Overview", "#/harness"],
  ["permissions", "Permissions", "#/harness/edit/permissions"],
  ["memory", "Memory", "#/harness/edit/memory"],
  ["sees", "What the model sees", "#/harness/sees"],
  ["versions", "Versions", "#/harness/versions"],
];

export function tabs(on) {
  return h("nav", { class: "hx-tabs", "aria-label": "Optimize the harness" },
    TABS.map(([k, l, href]) => h("a", { href, class: k === on ? "on" : null, "aria-current": k === on ? "page" : null }, l)));
}

const LEVEL_WORDS = { counted: "counted", recomputed: "recomputed", injected: "injected", estimated: "estimated", recorded: "recorded", none: "no data" };
/** An evidence level, as a small mono tag. */
export function lv(level) {
  return h("span", { class: `hx-lv ${level || ""}`, title: LEVEL_TITLES[level] || null }, LEVEL_WORDS[level] || level || "");
}
const LEVEL_TITLES = {
  counted: "Counted from rows Workbench recorded: exact over the window.",
  recomputed: "Recomputed from ChloeCtx's receipts: exact for what is given, not for what the model then does.",
  injected: "Text Workbench or ChloeCtx recorded giving the model.",
  estimated: "Estimated: Claude Code assembles it and does not record it.",
};

/** A scale of ticks: `on` of `total`, drawn as `n` ticks. */
export function ticks(on, total, tone = "", n = 24) {
  const k = total > 0 ? Math.max(on > 0 ? 1 : 0, Math.round((Math.min(on, total) / total) * n)) : 0;
  return h("div", { class: `ticks hx-ticks ${tone}`, "aria-hidden": "true" }, Array.from({ length: n }, (_, i) => h("i", { class: i < k ? "on" : null })));
}

/** A gauge of the scorecard: a label, a big mono figure, ticks, a line under. */
export function gauge({ label: l, v, unit, on = 0, total = 0, tone = "", delta, dtone = "" }) {
  return h("div", { class: "gauge hx-gauge" },
    h("div", { class: "lbl" }, l),
    h("div", { class: "v" }, h("b", { class: tone }, v), unit ? h("span", {}, unit) : null),
    ticks(on, total, tone),
    delta ? h("div", { class: `hx-dl ${dtone}` }, delta) : null);
}

export function fmtMs(ms) {
  ms = Math.max(0, Math.round(ms || 0));
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${s % 60 ? " " + String(s % 60).padStart(2, "0") + "s" : ""}`;
  return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
}

export function fmtTok(n) {
  n = n || 0;
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)}M`;
  if (n >= 1e4) return `${Math.round(n / 1000)}k`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

export function fmtNum(n) { return (n || 0).toLocaleString("en"); }

/** "07:23" today, else "Nov 12". */
export function fmtWhen(t) {
  const ms = toMs(t);
  if (!ms) return "–";
  const d = new Date(ms);
  return d.toDateString() === new Date().toDateString()
    ? d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" })
    : d.toLocaleDateString("en", { month: "short", day: "numeric" });
}

/** Text with `code` spans. */
export function rich(text) {
  return String(text || "").split("`").map((s, i) => (i % 2 ? h("code", {}, s) : s));
}

/** The repository query of the page: `repo=` when the shell gives one. */
export function repoQ(ctx) {
  const r = ctx.params && ctx.params.repo;
  return r ? `repo=${encodeURIComponent(r)}` : "";
}
export function repoOfCtx(ctx) { return (ctx.params && ctx.params.repo) || undefined; }

export function repoName(path) { return String(path || "").split("/").filter(Boolean).pop() || "repository"; }

/** A short-lived note at the bottom of the screen. */
export function toast(text, tone = "") {
  const t = h("div", { class: `hx-toast ${tone}`, role: "status" }, text);
  document.body.append(t);
  setTimeout(() => t.remove(), 3200);
}

/** A page that failed to load, with the server's words. */
export function failed(err) {
  return h("div", { class: "hx-failed" }, icon("warn"), h("div", {}, h("b", {}, "This page did not load."), h("p", {}, (err && err.message) || String(err))));
}

export function loading(text = "Reading this week's sessions…") {
  return h("div", { class: "hx-loading" }, text);
}

/** A segmented control: `seg([[key, label]], on, pick)`. */
export function seg(items, on, pick, { label: aria, disabled = [] } = {}) {
  return h("div", { class: "hx-seg", role: "radiogroup", "aria-label": aria || null },
    items.map(([k, l]) => h("button", { type: "button", role: "radio", "aria-checked": String(k === on), class: k === on ? "on" : null, disabled: disabled.includes(k) || null, onclick: () => pick(k) }, l)));
}

/** "No detectable difference" and its kin, as a result in its own right. */
export function resultLine(result, text) {
  const word = { no_difference: "no difference", no_detectable_difference: "no detectable difference", changed: "changed", higher: "higher", lower: "lower", too_few: "too few sessions", none: "nothing to compare" }[result] || result;
  return h("div", { class: `hx-result ${result}` }, h("span", { class: "hx-rw" }, word), h("span", {}, text));
}

useStyle("harness-parts", `
.hx{min-height:100%}
.hx code{font-family:var(--mono);font-size:.92em}
.hx-tabs{display:flex;gap:var(--s-5);border-bottom:var(--hair) solid var(--line);margin:0 0 var(--s-6);overflow-x:auto;scrollbar-width:none}
.hx-tabs::-webkit-scrollbar{display:none}
.hx-tabs a{flex:none;display:inline-flex;align-items:center;min-height:40px;color:var(--text-2);text-decoration:none;font-size:var(--fs-md);border-bottom:1.5px solid transparent;margin-bottom:-1px}
.hx-tabs a.on{color:var(--text);border-bottom-color:var(--text);font-weight:var(--w-mid)}
.hx-tabs a:hover{color:var(--text)}
.hx-lv{display:inline-block;font:var(--w-mid) var(--fs-xs)/18px var(--mono);letter-spacing:.06em;text-transform:uppercase;padding:0 6px;border-radius:var(--r-xs);border:var(--hair) solid var(--line-2);color:var(--text-2);white-space:nowrap}
.hx-lv.counted{color:var(--good-ink);border-color:var(--good)}
.hx-lv.recomputed,.hx-lv.injected{color:var(--accent-text);border-color:var(--accent-line)}
.hx-lv.estimated{border-style:dashed}
.hx-gauge{min-width:0}
.hx-gauge .v b.warn{color:var(--warn-ink)}.hx-gauge .v b.bad{color:var(--crit-ink)}.hx-gauge .v b.good{color:var(--good-ink)}
.hx-gauge .v span{font-size:var(--fs-sm);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.hx-gauge .v b{font-size:24px;line-height:32px}
.hx-ticks.warn i.on{background:var(--warn)}.hx-ticks.bad i.on{background:var(--crit)}.hx-ticks.good i.on{background:var(--good)}
.hx-dl{margin-top:var(--s-2);font:var(--w-text) var(--fs-xs)/16px var(--mono);color:var(--text-2)}
.hx-dl.good{color:var(--good-ink)}.hx-dl.bad{color:var(--crit-ink)}.hx-dl.warn{color:var(--warn-ink)}
.hx-sec{margin-top:var(--s-6)}
.hx-sec>.lbl{margin-bottom:var(--s-3)}
.hx-mut{color:var(--text-2)}
.hx-num{font-family:var(--mono);font-variant-numeric:tabular-nums}
.hx-toast{position:fixed;left:50%;bottom:24px;transform:translateX(-50%);z-index:90;background:var(--text);color:var(--surface);padding:10px 16px;border-radius:var(--r-sm);font-size:var(--fs-sm);box-shadow:var(--shadow-2);max-width:calc(100vw - 32px)}
.hx-toast.bad{background:var(--crit);color:var(--surface)}
.hx-failed{display:flex;gap:var(--s-3);padding:var(--s-6);color:var(--crit-ink)}
.hx-failed p{margin:var(--s-1) 0 0;color:var(--text-2)}
.hx-loading{padding:var(--s-7);color:var(--text-2)}
.hx-seg{display:inline-flex;padding:2px;border-radius:var(--r-sm);background:var(--line);gap:2px;flex-wrap:wrap}
.hx-seg button{min-height:28px;padding:0 10px;border:0;border-radius:var(--r-xs);background:none;font:var(--w-mid) var(--fs-sm) var(--sans);color:var(--text-2);cursor:pointer;white-space:nowrap}
.hx-seg button.on{background:var(--surface);color:var(--text);box-shadow:0 0 0 .5px var(--line-2)}
.hx-seg button:disabled{color:var(--text-3);cursor:not-allowed}
.hx-result{display:flex;gap:var(--s-3);align-items:baseline;padding:var(--s-3) var(--s-4);font-size:var(--fs-sm);line-height:var(--lh-sm);border-top:var(--hair) solid var(--line)}
.hx-result:first-child{border-top:0}
.hx-rw{flex:none;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:.06em;text-transform:uppercase;color:var(--text-2);padding:1px 6px;border-radius:var(--r-xs);background:var(--surface-3)}
.hx-result.higher .hx-rw,.hx-result.changed .hx-rw{color:var(--accent-text);background:var(--accent-soft)}
.hx-result.lower .hx-rw{color:var(--warn-ink);background:var(--warn-soft)}
.hx-flag{display:inline-flex;align-items:center;gap:4px;font:var(--w-mid) var(--fs-xs)/16px var(--mono);letter-spacing:.06em;text-transform:uppercase;color:var(--warn-ink)}
.hx-flag svg.i{width:13px;height:13px}
.hx-note{font-size:var(--fs-sm);line-height:var(--lh-sm);color:var(--text-2);margin:var(--s-2) 0 0}
.hx-grid2{display:grid;grid-template-columns:minmax(0,1.15fr) minmax(0,1fr);gap:var(--s-4);align-items:start}
@media (max-width:1024px){.hx-grid2{grid-template-columns:minmax(0,1fr)}}
@media (max-width:640px){.hx-tabs{gap:var(--s-4);margin-bottom:var(--s-4)}.hx-tabs a{min-height:var(--touch);font-size:var(--fs-body)}.hx-seg button{min-height:var(--touch)}}
`);

/** Saves a draft from `parent` (a config version's id) with `parts`
 *  replaced: `{permissions: edits}`, `{memory: edits}`, `{hooks: {timing}}`.
 *  Answers `{version, edits}`. */
export async function saveDraft(ctx, repo, parent, parts, note) {
  return ctx.api("/api/harness/versions", { method: "POST", body: { repo, parent, parts, note } });
}

/** Promotes a version; on 409 `widens` asks for the patterns by name
 *  (`confirm(patterns, message)` answers the ones the user named, or null). */
export async function promote(ctx, id, confirm = confirmWidening) {
  const url = `/api/harness/versions/${encodeURIComponent(id)}/promote`;
  try {
    return await ctx.api(url, { method: "POST", body: {} });
  } catch (err) {
    if (err.code !== "widens") throw err;
    let patterns = [];
    try { patterns = JSON.parse((String(err.message).match(/approve_widening: (\[.*\])$/) || [])[1] || "[]"); } catch { patterns = []; }
    const named = await confirm(patterns, err.message);
    if (!named) return null;
    return ctx.api(url, { method: "POST", body: { approve_widening: named } });
  }
}

/** The confirm a widening needs: each pattern ticked by name. */
export function confirmWidening(patterns, message) {
  return new Promise((resolve) => {
    const boxes = patterns.map((p) => h("input", { type: "checkbox", value: p }));
    const go = h("button", { type: "button", class: "btn primary", disabled: true }, "Promote: it widens what agents may do");
    const sync = () => { go.disabled = !boxes.every((b) => b.checked); };
    boxes.forEach((b) => b.addEventListener("change", sync));
    let scrim = null;
    const close = (v) => { scrim.remove(); resolve(v); };
    go.addEventListener("click", () => close(patterns));
    scrim = h("div", { class: "hx-scrim", onclick: (e) => { if (e.target === scrim) close(null); } },
      h("div", { class: "hx-sheet", role: "dialog", "aria-modal": "true", "aria-label": "Confirm widening" },
        h("div", { class: "hx-flag" }, icon("warn"), "widens"),
        h("h2", {}, "New agents would be allowed more"),
        h("p", { class: "hx-note" }, rich(String(message || "").replace(/ To promote it.*$/, ""))),
        h("p", { class: "hx-note" }, "Tick each pattern to confirm it by name. Nothing of yours is written: only the settings Workbench gives each new agent."),
        h("div", { class: "hx-pats" }, patterns.map((p, i) => h("label", {}, boxes[i], h("code", {}, p)))),
        h("div", { class: "hx-acts" }, go, h("button", { type: "button", class: "btn", onclick: () => close(null) }, "Not now"))));
    scrim.addEventListener("keydown", (e) => { if (e.key === "Escape") close(null); });
    document.body.append(scrim);
    (boxes[0] || go).focus();
  });
}

useStyle("harness-sheet", `
.hx-scrim{position:fixed;inset:0;z-index:85;background:var(--scrim);display:grid;place-items:center;padding:var(--s-4)}
.hx-sheet{background:var(--surface);border-radius:var(--r);box-shadow:var(--shadow-3);padding:var(--s-5);width:min(520px,100%);max-height:calc(100vh - 32px);overflow:auto}
.hx-sheet h2{margin:var(--s-2) 0 0;font:var(--w-title) var(--fs-lg)/var(--lh-lg) var(--sans)}
.hx-pats{display:flex;flex-direction:column;gap:var(--s-2);margin:var(--s-4) 0}
.hx-pats label{display:flex;gap:var(--s-3);align-items:center;min-height:var(--touch);padding:0 var(--s-3);border:var(--hair) solid var(--warn-line);border-radius:var(--r-sm);background:var(--warn-soft);cursor:pointer;overflow-wrap:anywhere}
.hx-pats input{width:18px;height:18px;flex:none;accent-color:var(--warn)}
.hx-acts{display:flex;gap:var(--s-2);flex-wrap:wrap}
`);
