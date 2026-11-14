// Model routing (SPEC.md, "Fourth build", row 6): in the New agent dialog,
// which model tasks like this one did best on in this repository, from
// `GET /api/route?repo=&task=` (BM25 over past tasks' text in Workbench's
// ChloeDB, weighed by how often each model finished them and what it cost).
// "Use" fills the Model field; the hint never changes it by itself.
//
//   attach(dialog, { api }) once, from app.js: it finds the dialog's task,
//   repository and model fields and keeps the hint under them.
import { h, useStyle, fmtTokens } from "./common.js";

const enc = encodeURIComponent;
const NAMES = { haiku: "Haiku", sonnet: "Sonnet", opus: "Opus", default: "the default" };
const nice = (m) => NAMES[m] || m;
// Sixth build: no cost recorded is no cost shown, not "$0.00"; with fewer
// than 5 similar tasks there is no suggestion, only why (src/routing.rs).
const usd = (x) => (x == null || !(x > 0) ? "" : x < 0.995 ? `$${x.toFixed(2)}` : `$${x.toFixed(x < 9.95 ? 2 : 0)}`);

export function attach(dialog, { api }) {
  if (!dialog || dialog.dataset.routeHint) return;
  dialog.dataset.routeHint = "1";
  const task = dialog.querySelector("#f-task");
  const repo = dialog.querySelector("#f-repo");
  const model = dialog.querySelector("#f-model");
  if (!task || !model) return;
  const host = h("div", { class: "rh", hidden: true, "aria-live": "polite" });
  const field = model.closest(".two") || model.closest(".field");
  field.after(host);
  let t = null, n = 0, last = "";

  async function refresh() {
    const text = task.value.trim();
    const r = (repo && repo.value.trim()) || "";
    const key = `${r}\u0001${text}`;
    if (key === last) return;
    last = key;
    if (text.split(/\s+/).filter(Boolean).length < 3 || !r.startsWith("/")) { host.hidden = true; return; }
    const my = ++n;
    host.hidden = false;
    host.replaceChildren(h("div", { class: "rh-row rh-dim" }, h("i", { class: "rh-spin" }), "Looking at similar past tasks…"));
    try {
      const v = await api(`/api/route?repo=${enc(r)}&task=${enc(text)}`);
      if (my !== n) return;
      render(v);
    } catch {
      if (my === n) host.hidden = true;
    }
  }

  function render(v) {
    if (!v || v.available === false) { host.hidden = true; return; }
    const alts = v.alternatives || [];
    const s = v.suggestion;
    if (!s || !alts.length) {
      host.replaceChildren(h("div", { class: "rh-row rh-dim" }, routeIcon(), v.why || "No past task of this repository is like this one yet: the model is your choice."));
      return;
    }
    const chosen = model.value.trim();
    const use = h("button", { type: "button", class: "btn sm rh-use" + (chosen === s.model ? " on" : ""), onclick: () => { model.value = s.model; model.dispatchEvent(new Event("input", { bubbles: true })); render(v); } },
      chosen === s.model ? `Using ${nice(s.model)}` : `Use ${nice(s.model)}`);
    host.replaceChildren(...[
      h("div", { class: "rh-top" },
        routeIcon(),
        h("div", { class: "rh-main" },
          h("div", { class: "rh-line" }, "Suggested: ", h("b", {}, nice(s.model)), h("span", { class: "rh-sep" }, "·"),
            h("span", {}, `${s.done} of ${s.n} like this finished`), usd(s.avg_cost_usd) ? [h("span", { class: "rh-sep" }, "·"), h("span", {}, `${usd(s.avg_cost_usd)} avg`)] : null,
            s.avg_minutes != null ? [h("span", { class: "rh-sep" }, "·"), h("span", {}, `${Math.round(s.avg_minutes)} min`)] : null),
          h("div", { class: "rh-why" }, v.why || "")),
        use),
      h("div", { class: "rh-alts" }, alts.map((a) => h("button", {
        type: "button", class: "rh-alt" + (a.model === s.model ? " best" : ""), title: `${a.done} of ${a.n} finished${usd(a.avg_cost_usd) ? ` · ${usd(a.avg_cost_usd)} avg` : ""}${a.avg_tokens ? ` · ${fmtTokens(a.avg_tokens)} tokens` : ""}`,
        onclick: () => { model.value = a.model === "default" ? "" : a.model; render(v); },
      },
        h("b", {}, nice(a.model)),
        h("span", { class: "rh-meter" }, h("i", { style: { width: Math.round((a.done / Math.max(1, a.n)) * 100) + "%" } })),
        h("span", {}, `${a.done}/${a.n}`), h("span", { class: "rh-dim" }, usd(a.avg_cost_usd))))),
      v.apply === false ? h("div", { class: "rh-foot rh-dim" }, `${v.similar} similar past task${v.similar === 1 ? "" : "s"}: an agent's spawn_agent keeps the default model until there are 5.`)
        : (v.examples || []).length ? h("div", { class: "rh-foot" }, h("span", { class: "rh-dim" }, "Like "), v.examples.slice(0, 2).map((e, i) => [i ? h("span", { class: "rh-dim" }, ", ") : null,
          h("span", { class: "rh-ex", title: `${e.name}: ${e.model}, ${e.done ? "finished" : "not finished"}${usd(e.cost_usd) ? `, ${usd(e.cost_usd)}` : ""}` }, `“${clip(e.task, 54)}”`, h("span", { class: "rh-dim" }, ` ${nice(e.model)}${usd(e.cost_usd) ? `, ${usd(e.cost_usd)}` : ""}`))]),
          h("span", { class: "rh-dim" }, ` · from ${v.similar} past tasks in this repository`)) : null].flat().filter((k) => k != null && k !== false));
  }

  const soon = () => { clearTimeout(t); t = setTimeout(refresh, 350); };
  task.addEventListener("input", soon);
  repo?.addEventListener("input", soon);
  repo?.addEventListener("change", soon);
  model.addEventListener("input", () => { if (!host.hidden && host.querySelector(".rh-use")) { last = ""; refresh(); } });
  // The dialog clears its fields when it opens.
  new MutationObserver(() => { if (dialog.open) { last = ""; host.hidden = true; soon(); } }).observe(dialog, { attributes: true, attributeFilter: ["open"] });
}

function clip(s, n) { s = String(s || ""); return s.length > n ? s.slice(0, n - 1) + "…" : s; }

function routeIcon() {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 16 16"); s.setAttribute("class", "rh-ic"); s.setAttribute("aria-hidden", "true");
  s.innerHTML = '<circle cx="3.5" cy="12.5" r="1.8" fill="none" stroke="currentColor" stroke-width="1.4"/><circle cx="12.5" cy="3.5" r="1.8" fill="none" stroke="currentColor" stroke-width="1.4"/><path d="M5.2 12.5h3.3a2 2 0 0 0 2-2V5.3" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>';
  return s;
}

useStyle("route-hint", `
.rh{margin:2px 0 4px;border:1px solid color-mix(in srgb,var(--accent) 35%,var(--line));background:color-mix(in srgb,var(--accent) 6%,var(--surface));border-radius:10px;padding:10px 12px;display:flex;flex-direction:column;gap:8px;font-size:12.5px}
.rh[hidden]{display:none}
.rh-row{display:flex;align-items:center;gap:8px}
.rh-top{display:flex;align-items:flex-start;gap:10px}
.rh-ic{width:16px;height:16px;color:var(--accent);flex:none;margin-top:1px}
.rh-main{flex:1;min-width:0}
.rh-line{font-size:13px;color:var(--text);display:flex;flex-wrap:wrap;align-items:baseline;gap:0 6px}
.rh-line b{font-weight:700}
.rh-sep{color:var(--muted)}
.rh-why{color:var(--text-2);margin-top:2px;line-height:1.45}
.rh-dim{color:var(--muted)}
.btn.rh-use{flex:none}
.btn.rh-use.on{background:var(--accent-soft);color:var(--accent);border-color:color-mix(in srgb,var(--accent) 40%,var(--line))}
.rh-alts{display:flex;flex-wrap:wrap;gap:6px;padding-left:26px}
.rh-alt{display:inline-flex;align-items:center;gap:7px;height:26px;padding:0 10px;border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--text-2);font:inherit;font-size:12px;cursor:pointer;font-variant-numeric:tabular-nums}
.rh-alt:hover{border-color:var(--line-2);color:var(--text)}
.rh-alt b{font-weight:650;color:var(--text)}
.rh-alt.best{border-color:color-mix(in srgb,var(--accent) 50%,var(--line))}
.rh-meter{width:34px;height:5px;border-radius:999px;background:var(--surface-3);overflow:hidden}
.rh-meter>i{display:block;height:100%;background:var(--good);border-radius:999px}
.rh-foot{padding-left:26px;font-size:12px;color:var(--text-2);line-height:1.5}
.rh-ex{color:var(--text-2)}
.rh-spin{width:11px;height:11px;border-radius:50%;border:2px solid var(--line-2);border-top-color:var(--accent);animation:rh-spin .8s linear infinite}
@keyframes rh-spin{to{transform:rotate(360deg)}}
`);
