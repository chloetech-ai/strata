// The agent page (JOURNEYS.md, "The agent page"): one header (who, its
// state in the shared words, branch, model, +/−, task), the question it
// asks first, then the tabs Progress / Changes / Preview / Terminal. The
// terminal and the diff stay app.js's (they hold sockets and a cursor);
// this module draws the parts around them and mounts the story and the
// preview into their tabs. On a phone the terminal is read-only until
// Type, and the banner says which.
import { h, useStyle, btn, kc, icon, stateDot, since, avatar, churnBar } from "/levels/common.js";

export const TABS = [["progress", "Progress"], ["changes", "Changes"], ["preview", "Preview"], ["terminal", "Terminal"], ["events", "Events"]];

/** The header. `o`: {group, project, stat: {files, added, removed}, onStop, onRestart, onTell, ship} */
export function head(a, o = {}) {
  useStyle("g-agent", CSS);
  const g = o.group || (a.running ? "working" : "finished");
  const word = g === "needs" ? "needs you" : g === "working" ? (a.state === "starting" ? "starting" : "working") : g === "ready" ? "ready to review" : a.running ? "idle" : "finished";
  const st = o.stat;
  const primary = g === "ready" && o.ship
    ? btn("Review and ship", { primary: true, href: o.ship, icon: "ship" })
    : a.running && o.onTell ? btn(`Tell ${a.name}…`, { primary: g !== "needs", icon: "send", onclick: o.onTell }) : null;
  const menu = h("details", { class: "ag-menu" },
    h("summary", { class: "btn", "aria-label": "More for this agent" }, a.running ? [icon("stop", "i sm"), "Stop"] : [icon("rewind", "i sm"), "Restart"], icon("down", "i sm")),
    h("div", { class: "ag-mlist", role: "menu" },
      a.running ? h("button", { type: "button", role: "menuitem", onclick: (e) => { e.target.closest("details").open = false; o.onStop && o.onStop(); } },
        h("b", {}, "Stop"), h("span", {}, "Ends the session (asks first). Its worktree and branch stay.")) : null,
      !a.running ? h("button", { type: "button", role: "menuitem", onclick: (e) => { e.target.closest("details").open = false; o.onRestart && o.onRestart(); } },
        h("b", {}, "Restart with the same task"), h("span", {}, "Same name, worktree and task, continuing its session when Claude Code still has it.")) : null,
      h("a", { role: "menuitem", href: `#/a/${a.id}/events` }, h("b", {}, "Events"), h("span", {}, "Every hook event, as it came."))));
  return h("div", { class: "ag-head" },
    avatar(a, 28),
    h("div", { class: "ag-t" },
      h("h1", {}, a.name, stateDot(g, word), o.project ? h("span", { class: "ag-proj" }, o.project) : null),
      a.task ? h("p", { class: "ag-task", title: a.task }, a.task) : h("p", { class: "ag-task" }, "No task: it started at a prompt."),
      h("div", { class: "ag-meta" },
        a.branch ? h("span", {}, icon("branch", "i sm"), a.branch) : null,
        a.model ? h("span", {}, a.model) : null,
        st && (st.added || st.removed) ? h("span", {}, churnBar(st.added, st.removed), st.files ? ` · ${st.files} file${st.files === 1 ? "" : "s"}` : "") : null,
        a.state_since ? h("span", { class: "num" }, `${word} for ${since(a.state_since)}`) : null,
        a.read_only ? h("span", {}, "read-only") : null)),
    h("div", { class: "ag-act" }, primary, menu));
}

/** The tab links. `cur` is one of TABS' keys; `n` the changed files' count. */
export function tabs(a, cur, { n, preview } = {}) {
  return [
    ...TABS.filter(([k]) => k !== "preview" || preview !== false).map(([k, t]) => h("a", {
      role: "tab", href: `#/a/${a.id}${k === "progress" ? "" : "/" + k}`, "aria-selected": String(k === cur), class: k === "events" ? "narrow-tab" : null,
    }, t, k === "changes" ? h("span", { class: "n" }, n ? String(n) : "") : null)),
    h("span", { class: "hint" }, cur === "terminal" ? [kc("Ctrl-b"), kc("Esc"), " leaves the terminal"] : [kc("g t"), " terminal"]),
  ];
}

/** The banner over the terminal. On a phone: read-only until Type. */
export function termNote(a, { phone, typing, onType, onDone } = {}) {
  if (!a.running) return [h("span", {}, h("b", {}, "The session has ended."), " Its scrollback is below.")];
  if (!phone) return [h("span", {}, "Every key goes to ", h("b", {}, a.name), ", Esc included."), h("span", { class: "sp" }), kc("Ctrl-b"), kc("Esc"), h("span", {}, " leaves")];
  return typing
    ? [h("span", {}, "Typing to ", h("b", {}, a.name), ". Done to stop."), h("span", { class: "sp" }), btn("Done", { sm: true, onclick: onDone })]
    : [h("span", {}, "Read only. Nothing you tap reaches ", h("b", {}, a.name), "."), h("span", { class: "sp" }), btn("Type", { sm: true, primary: true, onclick: onType })];
}

/** Mounts the Progress (the story, embedded) or the Preview into `host`. */
export async function mountTab(kind, host, ctx) {
  host.replaceChildren(h("div", { class: "lv-loading" }, "Loading…"));
  const m = await import(kind === "preview" ? "/levels/preview.js" : "/levels/story.js");
  host.replaceChildren();
  return m.mount(host, { ...ctx, params: { ...(ctx.params || {}), embed: true } });
}

const CSS = `
.ag-head h1 .st{font:var(--w-mid) var(--fs-sm)/var(--lh-sm) var(--sans);letter-spacing:0}
.ag-proj{font:var(--w-text) var(--fs-sm)/var(--lh-sm) var(--mono);color:var(--text-2)}
.ag-meta span{display:inline-flex;align-items:center;gap:4px}
.ag-menu{position:relative}
.ag-menu>summary{list-style:none;cursor:pointer}
.ag-menu>summary::-webkit-details-marker{display:none}
.ag-mlist{position:absolute;right:0;top:calc(100% + 6px);z-index:15;width:300px;background:var(--surface);border:var(--hair) solid var(--line-2);border-radius:var(--r);box-shadow:var(--shadow-2);padding:var(--s-1)}
.ag-mlist>*{display:flex;flex-direction:column;gap:2px;width:100%;text-align:left;padding:var(--s-2) var(--s-3);border:0;background:none;border-radius:var(--r-sm);color:var(--text);text-decoration:none;font:inherit;cursor:pointer}
.ag-mlist>*:hover,.ag-mlist>*:focus-visible{background:var(--surface-2)}
.ag-mlist b{font-weight:var(--w-mid)}
.ag-mlist span{color:var(--text-2);font-size:var(--fs-sm)}
.ag-tabs .narrow-tab{display:none}
@media (max-width:640px){.ag-tabs .narrow-tab{display:inline-flex}.ag-head>.lv-av{display:none}.ag-head .ag-act{width:100%}.ag-head .ag-act>.btn{flex:1}.ag-mlist{width:min(300px,calc(100vw - 32px))}}
`;
