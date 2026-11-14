// Agent lifecycle pieces (SPEC.md, "Sixth build: E"), shared by the zoom
// view, the stop dialog, the New agent dialog and Preview:
//
// - serversBox: the servers an agent started (GET /api/servers?agent=),
//   each with its command, port and age, and Stop
//   (POST /api/servers/:pid/stop);
// - stopChoice: in the stop dialog, whether its servers stop with it, and
//   "Remember" (PUT /api/servers/policy {on_stop});
// - attachRepoBox: the repository box, which completes paths
//   (GET /api/repos/dirs?q= → {dirs: [{path, name, git}]}), git
//   repositories first, recent ones before both, and shows the whole path.
import { h, useStyle, fmtAgo, later } from "./common.js";

const enc = encodeURIComponent;

/** The agent's servers as a box that refreshes itself while it is on the page. */
export function serversBox(api, agentId, { title = "Servers", empty = null, onChange = null } = {}) {
  const el = h("div", { class: "lc-servers" });
  let list = [], timer = 0, stopping = new Set();
  async function refresh() {
    if (!el.isConnected && timer) { clearInterval(timer); timer = 0; return; }
    try {
      const d = await api(`/api/servers?agent=${enc(agentId)}`);
      list = (d && d.servers) || [];
    } catch { list = []; }
    draw();
    onChange?.(list);
  }
  function draw() {
    if (!list.length) { el.replaceChildren(...(empty ? [h("div", { class: "lc-h" }, title), h("div", { class: "lc-none" }, empty)] : [])); return; }
    el.replaceChildren(
      h("div", { class: "lc-h" }, title, h("span", { class: "lc-n" }, String(list.length))),
      ...list.map((s) => h("div", { class: "lc-srv" },
        h("span", { class: "lc-port" }, `:${s.port ?? (s.ports || [])[0] ?? "?"}`),
        h("code", { class: "lc-cmd", title: s.command || "" }, s.command || `pid ${s.pid}`),
        h("span", { class: "lc-age" }, s.since_ms ? fmtAgo(s.since_ms).replace(" ago", "") : ""),
        h("button", {
          class: "btn sm lc-stop", type: "button", disabled: stopping.has(s.pid) ? true : null,
          title: `Stop pid ${s.pid} (SIGTERM, then SIGKILL after 3 s)`, "aria-label": `Stop the server on port ${s.port}`,
          onclick: async () => {
            stopping.add(s.pid); draw();
            try { await api(`/api/servers/${enc(s.pid)}/stop`, { method: "POST" }); } catch { /* it is gone or not ours: the refresh says */ }
            setTimeout(() => { stopping.delete(s.pid); refresh(); }, 600);
          },
        }, stopping.has(s.pid) ? "Stopping…" : "Stop"))));
  }
  refresh();
  timer = setInterval(refresh, 5000);
  return { el, refresh, servers: () => list, destroy: () => { clearInterval(timer); timer = 0; } };
}

/**
 * The stop dialog's choice for an agent's servers. `host` is emptied and
 * filled; `choice()` answers true (stop them), false (keep them) or
 * undefined (the agent has none: the server's policy applies).
 */
export async function stopChoice(api, agentId, host) {
  host.replaceChildren();
  host.hidden = true;
  let d;
  try { d = await api(`/api/servers?agent=${enc(agentId)}`); } catch { return { choice: () => undefined }; }
  const list = (d && d.servers) || [];
  if (!list.length) return { choice: () => undefined };
  const policy = (d && d.policy) || "";
  const stop = h("input", { type: "checkbox", id: "lc-stop-too", checked: policy !== "keep" ? true : null });
  const remember = h("input", { type: "checkbox", id: "lc-remember", checked: policy ? true : null });
  host.append(
    h("div", { class: "lc-h" }, `It started ${list.length === 1 ? "a server" : `${list.length} servers`}`),
    ...list.map((s) => h("div", { class: "lc-srv" }, h("span", { class: "lc-port" }, `:${s.port}`), h("code", { class: "lc-cmd", title: s.command || "" }, s.command || `pid ${s.pid}`))),
    h("label", { class: "check lc-ck", for: "lc-stop-too" }, stop, h("span", null, h("b", null, "Stop them too"), h("span", null, "Off: they keep running after the agent ends, and stay in its Servers list."))),
    h("label", { class: "check lc-ck", for: "lc-remember" }, remember, h("span", null, h("b", null, "Remember"), h("span", null, "Do this for every agent from now on, without asking."))));
  host.hidden = false;
  return {
    choice: () => {
      const v = !!stop.checked;
      const on = remember.checked ? (v ? "stop" : "keep") : "";
      if (on !== policy) api("/api/servers/policy", { method: "PUT", body: { on_stop: on } }).catch(() => {});
      return v;
    },
  };
}

/**
 * Turns the repository box into a path completer. `recent()` gives the
 * recent repositories. The box is a one-row textarea that grows, so a long
 * path shows whole; Enter picks the highlighted path (or moves on).
 */
export function attachRepoBox(input, { api, recent = () => [], next = null }) {
  if (!input || input.dataset.lc) return;
  input.dataset.lc = "1";
  const dd = h("div", { class: "lc-dd", role: "listbox", hidden: true });
  const wrap = h("div", { class: "lc-wrap" });
  input.before(wrap);
  wrap.append(input, dd);
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-autocomplete", "list");
  input.setAttribute("aria-expanded", "false");
  let items = [], sel = -1, req = 0, off = false;
  const grow = () => { input.style.height = "auto"; input.style.height = `${input.scrollHeight + 2}px`; };
  const close = () => { dd.hidden = true; input.setAttribute("aria-expanded", "false"); sel = -1; };
  function show() {
    if (!items.length || document.activeElement !== input) { close(); return; }
    dd.replaceChildren(...items.map((it, i) => h("div", {
      class: "lc-opt" + (i === sel ? " sel" : ""), role: "option", "aria-selected": String(i === sel),
      onmousedown: (e) => { e.preventDefault(); pick(i); },
    },
    h("span", { class: "lc-kind " + it.kind }, it.kind === "recent" ? "recent" : it.git ? "git" : "folder"),
    h("span", { class: "lc-path" }, it.path))));
    dd.hidden = false;
    input.setAttribute("aria-expanded", "true");
  }
  function pick(i) {
    const it = items[i];
    if (!it) return;
    input.value = it.git || it.kind === "recent" ? it.path : it.path.replace(/\/?$/, "/");
    input.dispatchEvent(new Event("input", { bubbles: true }));
    grow();
    if (it.git || it.kind === "recent") { close(); next?.(); } else suggest();
  }
  const suggest = later(async () => {
    const q = input.value.trim();
    const r = ++req;
    const rec = recent().filter((p) => !q || p.includes(q)).slice(0, 4).map((p) => ({ path: p, kind: "recent", git: true }));
    let dirs = [];
    if (!off && q) {
      try {
        const d = await api(`/api/repos/dirs?q=${enc(q)}`);
        dirs = ((d && d.dirs) || []).map((x) => ({ path: x.path, git: !!x.git, kind: x.git ? "git" : "dir" }));
      } catch (e) { if (e && e.status === 404) off = true; }
    }
    if (r !== req) return;
    const seen = new Set(rec.map((x) => x.path));
    dirs = dirs.filter((x) => !seen.has(x.path));
    // Git repositories first, then folders to go into.
    dirs.sort((a, b) => Number(b.git) - Number(a.git));
    items = [...rec, ...dirs].slice(0, 12);
    sel = items.length ? 0 : -1;
    show();
  }, 120);
  input.addEventListener("input", () => { grow(); suggest(); });
  input.addEventListener("focus", () => { grow(); suggest(); });
  input.addEventListener("blur", () => setTimeout(close, 80));
  input.addEventListener("keydown", (e) => {
    const open = !dd.hidden && items.length;
    if (e.key === "ArrowDown" && open) { e.preventDefault(); sel = (sel + 1) % items.length; show(); }
    else if (e.key === "ArrowUp" && open) { e.preventDefault(); sel = (sel - 1 + items.length) % items.length; show(); }
    else if ((e.key === "Tab" && open && !e.shiftKey) || (e.key === "Enter" && open && sel >= 0 && !e.ctrlKey && !e.metaKey)) { e.preventDefault(); pick(sel); }
    else if (e.key === "Enter" && !e.ctrlKey && !e.metaKey) { e.preventDefault(); close(); next?.(); }
    else if (e.key === "Escape" && open) { e.preventDefault(); e.stopPropagation(); close(); }
  });
  requestAnimationFrame(grow);
  return { grow, close };
}

useStyle("lifecycle", `
.lc-servers{display:flex;flex-direction:column;gap:5px}
.lc-servers:empty{display:none}
.lc-h{font:650 11px var(--sans);color:var(--muted);text-transform:uppercase;letter-spacing:.04em;display:flex;align-items:center;gap:6px;margin-top:2px}
.lc-n{font:700 10px var(--sans);background:var(--surface-2);color:var(--text-2);border-radius:999px;padding:0 6px;line-height:16px;text-transform:none}
.lc-none{font-size:12px;color:var(--muted)}
.lc-srv{display:grid;grid-template-columns:auto minmax(0,1fr) auto auto;align-items:center;gap:8px;font-size:12px;min-height:26px}
.lc-port{font:700 12px var(--mono);color:var(--good-ink,var(--text))}
.lc-cmd{font:11.5px var(--mono);color:var(--text-2);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0}
.lc-age{font-size:11px;color:var(--muted)}
.lc-ck{margin-top:6px}
#confirm-servers{display:flex;flex-direction:column;gap:4px;margin-top:10px;padding-top:10px;border-top:1px solid var(--line)}
#confirm-servers[hidden]{display:none}
.lc-wrap{position:relative}
.lc-dd{position:absolute;left:0;right:0;top:100%;margin-top:4px;z-index:5;background:var(--surface);border:1px solid var(--line-2);border-radius:9px;box-shadow:var(--shadow-3);max-height:300px;overflow-y:auto;padding:4px}
.lc-dd[hidden]{display:none}
.lc-opt{display:flex;align-items:baseline;gap:8px;padding:5px 8px;border-radius:6px;cursor:pointer}
.lc-opt.sel,.lc-opt:hover{background:var(--accent-soft)}
.lc-kind{flex:none;font:700 10px var(--sans);text-transform:uppercase;letter-spacing:.04em;color:var(--muted);width:44px}
.lc-kind.git{color:var(--accent)}
.lc-path{font:12px var(--mono);color:var(--text);overflow-wrap:anywhere;word-break:break-all}
textarea.lc-repo{resize:none;overflow:hidden;min-height:36px;height:36px;padding:8px 11px;font-family:var(--mono);font-size:13px;line-height:18px;overflow-wrap:anywhere;word-break:break-all;white-space:pre-wrap}
textarea.lc-repo[aria-invalid="true"]{border-color:var(--crit)}
`);
