// The setup check (SPEC.md, "ChloeCtx is required"): when the sessions
// Workbench starts would not get ChloeCtx, a sheet says what is missing and
// the one command that fixes it, and a bar stays until it is fixed.
import { h, useStyle } from "./common.js";

const CSS = `
.su-veil { position: fixed; inset: 0; z-index: 60; display: grid; place-items: center; background: color-mix(in srgb, var(--bg) 72%, transparent); backdrop-filter: blur(3px); padding: 16px; }
.su-card { width: min(560px, 100%); background: var(--surface); border: 1px solid var(--line); border-radius: var(--r); box-shadow: var(--shadow-3); padding: 22px 24px; }
.su-card h2 { margin: 0 0 6px; font-size: 18px; display: flex; gap: 10px; align-items: center; }
.su-card h2 .dia { color: var(--ctx, #7c3aed); font-size: 20px; }
.su-card p { margin: 8px 0; color: var(--text-2); line-height: 1.5; font-size: 14px; }
.su-card .why { color: var(--crit-ink); background: var(--crit-soft); border-radius: var(--r-sm); padding: 8px 10px; font-size: 13px; }
.su-card pre { margin: 10px 0; padding: 10px 12px; background: var(--sunken); border: 1px solid var(--line); border-radius: var(--r-sm); font: 13px/1.5 var(--mono); white-space: pre-wrap; word-break: break-all; }
.su-card ul { margin: 6px 0 4px 18px; padding: 0; color: var(--text-2); font-size: 13px; line-height: 1.6; }
.su-row { display: flex; gap: 8px; justify-content: flex-end; margin-top: 14px; flex-wrap: wrap; }
.su-bar { position: fixed; left: 50%; transform: translateX(-50%); bottom: 14px; z-index: 55; display: flex; gap: 10px; align-items: center; background: var(--warn-soft); color: var(--warn-ink); border: 1px solid var(--warn-line); border-radius: 999px; padding: 6px 8px 6px 14px; font-size: 13px; box-shadow: var(--shadow-2); max-width: calc(100vw - 32px); }
.su-bar b { font-weight: 600; }
`;

let veil = null, bar = null;

function clear() {
  veil?.remove(); veil = null;
  bar?.remove(); bar = null;
}

function showBar(api, st) {
  bar?.remove();
  bar = h("div", { class: "su-bar", role: "status" },
    h("span", null, h("b", null, "ChloeCtx is not set up."), " Agents cannot start until it is."),
    h("button", { class: "btn sm", onclick: () => { bar.remove(); bar = null; showSheet(api, st); } }, "How to fix"));
  document.body.append(bar);
}

function showSheet(api, st) {
  veil?.remove();
  const status = h("span", { class: "muted" });
  const again = h("button", { class: "btn primary", onclick: async () => {
    status.textContent = "Checking…";
    const next = await fetchStatus(api);
    if (next?.ok) { clear(); return; }
    status.textContent = next?.problem ? "Still missing." : "";
    if (next) render(next);
  } }, "Check again");
  const later = h("button", { class: "btn ghost", onclick: () => { veil.remove(); veil = null; showBar(api, st); } }, "Not now");
  const body = h("div");
  const render = (s) => {
    body.replaceChildren(
      h("p", { class: "why" }, s.problem || "ChloeCtx did not answer."),
      h("p", null, "Run this once, from a checkout of ChloeDB, then check again:"),
      h("pre", null, s.fix || "./install.sh --workbench"),
      h("p", null, "It builds ChloeCtx and Workbench, installs them to ~/.local/bin, downloads the 129 MB embedding model and checks them. It does not change your Claude Code settings: Workbench gives each session it starts ChloeCtx's hooks and memory tools itself."),
    );
  };
  render(st);
  veil = h("div", { class: "su-veil", role: "dialog", "aria-modal": "true", "aria-labelledby": "su-title" },
    h("div", { class: "su-card" },
      h("h2", { id: "su-title" }, h("span", { class: "dia", "aria-hidden": "true" }, "◆"), "Workbench needs ChloeCtx"),
      h("p", null, "ChloeCtx is the memory Workbench reads above the terminal:"),
      h("ul", null,
        h("li", null, "who decided what, where and when (Why?, the decision trail)"),
        h("li", null, "what each agent knew (Story), precedent for questions (Decide)"),
        h("li", null, "rules that reach every agent, and the digest while you were away")),
      body,
      h("div", { class: "su-row" }, status, later, again)));
  document.body.append(veil);
  again.focus();
}

async function fetchStatus(api) {
  try { return await api("/api/chloectx"); } catch { return null; }
}

/** Checks once at start; shows the sheet when sessions would miss ChloeCtx. */
export async function checkSetup(api) {
  useStyle("setup", CSS);
  const st = await fetchStatus(api);
  if (!st || st.ok || st.required === false) return;
  showSheet(api, st);
}
