// Repositories without an agent (sixth build, src/repos.rs): one picker that
// search, Map, Trace, Library and Health show wherever they would have said
// "repo is required". A path box that completes folders (git checkouts
// first, the whole path in view), the repositories Workbench knows (opened
// here or worked in by an agent), and Open. Opening one adds it
// (POST /api/repos, which also indexes it for code search) and tells the app
// (the window event `wb-repo`), which makes it the repository the screens
// read and opens the screen again on it.
//   repoPicker(ctx, opts): the picker, as a card.
//   repoSwitch(ctx, repo): a small button naming the repository, which opens
//     the picker in a popover to read another.
//   needsRepo(err): whether an error is "no repository was given".
import { h, useStyle, fmtAgo, later, onEscape } from "./common.js";

const RECENT_KEY = "chloedb-workbench-repos";
const OPEN_KEY = "chloedb-workbench-open-repo";
const base = (p) => String(p || "").split("/").filter(Boolean).pop() || p || "";

/** An error that says no repository was given (or one that is not a folder). */
export function needsRepo(err) {
  const m = String((err && (err.message || err)) || "");
  return (err && err.code === "repo_required") || /\brepo(sitory)? (or agent )?is required|agent or repo is required/i.test(m);
}

/** The repository opened in the picker last, if any. */
export function openedRepo() {
  try { return localStorage.getItem(OPEN_KEY) || null; } catch { return null; }
}

/** Makes `path` the repository the screens read: remembered, and the app told. */
export function chooseRepo(path) {
  try {
    const list = JSON.parse(localStorage.getItem(RECENT_KEY) || "[]");
    const next = [path, ...(Array.isArray(list) ? list : []).filter((r) => r !== path)].slice(0, 8);
    localStorage.setItem(RECENT_KEY, JSON.stringify(next));
    localStorage.setItem(OPEN_KEY, path);
  } catch { /* private mode: the event still carries it */ }
  window.dispatchEvent(new CustomEvent("wb-repo", { detail: path }));
}

let cache = null;
/** The repositories Workbench knows, most recent first (cached for 5 s). */
export async function knownRepos(ctx, fresh = false) {
  if (!fresh && cache && Date.now() - cache.at < 5000) return cache.list;
  const r = await ctx.api("/api/repos");
  cache = { at: Date.now(), list: r.repos || [] };
  return cache.list;
}

/** The picker. opts: { title, why, current, compact, onDone() } */
export function repoPicker(ctx, opts = {}) {
  const S = { q: "", dirs: [], sel: -1, known: null, err: null, busy: false, knownErr: null };
  const input = h("input", {
    class: "rp-in", type: "text", spellcheck: "false", autocomplete: "off", "aria-label": "The repository's folder",
    placeholder: "/home/you/src/shop  or  ~/src/shop", value: opts.current || "",
    "aria-autocomplete": "list", role: "combobox", "aria-expanded": "false",
  });
  const list = h("ul", { class: "rp-dirs", role: "listbox", hidden: true });
  const err = h("div", { class: "rp-err", role: "alert", hidden: true });
  const go = h("button", { class: "btn primary", type: "submit" }, "Open");
  const known = h("div", { class: "rp-known" });

  // The end of a long path is what one types at: keep it in view.
  const toEnd = () => { try { input.setSelectionRange(input.value.length, input.value.length); } catch {} input.scrollLeft = input.scrollWidth; };

  let req = 0;
  async function complete() {
    const q = input.value;
    const r = ++req;
    try {
      const d = await ctx.api(`/api/repos/dirs?q=${encodeURIComponent(q)}`);
      if (r !== req) return;
      S.dirs = d.dirs || [];
      S.sel = -1;
    } catch { if (r === req) S.dirs = []; }
    drawDirs();
  }
  const completeSoon = later(complete, 110);

  function drawDirs() {
    list.hidden = !S.dirs.length || document.activeElement !== input;
    input.setAttribute("aria-expanded", String(!list.hidden));
    list.replaceChildren(...S.dirs.map((d, i) => {
      const p = String(d.path);
      const cut = p.lastIndexOf("/") + 1;
      return h("li", {
        class: "rp-dir" + (i === S.sel ? " sel" : ""), role: "option", "aria-selected": String(i === S.sel), title: p,
        onmousedown: (e) => { e.preventDefault(); take(d, true); },
      },
      h("span", { class: "rp-dpath" }, h("span", { class: "rp-dhead" }, p.slice(0, cut)), h("b", null, p.slice(cut))),
      d.git ? h("span", { class: "rp-git", title: "a git checkout: Workbench can open it" }, "git") : null);
    }));
  }

  // A folder from the list: a git checkout opens on a click; Tab goes into it.
  function take(d, open) {
    input.value = String(d.path) + (d.git && open ? "" : "/");
    toEnd();
    if (d.git && open) submit();
    else { complete(); input.focus(); }
  }

  async function submit(e) {
    e && e.preventDefault();
    const path = input.value.trim().replace(/\/+$/, "") || "";
    if (!path) { input.focus(); return; }
    S.busy = true; go.disabled = true; err.hidden = true;
    try {
      const r = await ctx.api("/api/repos", { method: "POST", body: { path, ...(opts.body ? opts.body() : {}) } });
      cache = null;
      const repo = r.repo?.path || path;
      chooseRepo(repo);
      opts.onDone && opts.onDone(repo);
    } catch (x) {
      err.textContent = x.message || String(x);
      err.hidden = false;
      input.setAttribute("aria-invalid", "true");
      input.focus();
    }
    S.busy = false; go.disabled = false;
  }

  input.addEventListener("input", () => { input.removeAttribute("aria-invalid"); err.hidden = true; completeSoon(); });
  input.addEventListener("focus", () => { toEnd(); complete(); });
  input.addEventListener("blur", () => setTimeout(() => { list.hidden = true; input.setAttribute("aria-expanded", "false"); }, 120));
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" && S.dirs.length) { e.preventDefault(); S.sel = Math.min(S.dirs.length - 1, S.sel + 1); drawDirs(); list.children[S.sel]?.scrollIntoView({ block: "nearest" }); return; }
    if (e.key === "ArrowUp" && S.dirs.length) { e.preventDefault(); S.sel = Math.max(-1, S.sel - 1); drawDirs(); return; }
    if (e.key === "Tab" && !e.shiftKey && S.dirs.length && input.value) {
      e.preventDefault();
      take(S.dirs[Math.max(0, S.sel)], false);
      return;
    }
    if (e.key === "Enter" && S.sel >= 0 && S.dirs[S.sel]) { e.preventDefault(); take(S.dirs[S.sel], true); return; }
  });

  async function loadKnown() {
    try { S.known = await knownRepos(ctx, true); S.knownErr = null; } catch (x) { S.known = []; S.knownErr = x.message || String(x); }
    drawKnown();
  }
  function drawKnown() {
    const rows = (S.known || []).filter((r) => r.exists !== false);
    if (!S.known) { known.replaceChildren(h("div", { class: "rp-none" }, "Reading the repositories Workbench knows…")); return; }
    if (!rows.length) { known.replaceChildren(h("div", { class: "rp-none" }, S.knownErr || "None yet: the first one you open is remembered here.")); return; }
    known.replaceChildren(h("div", { class: "rp-kh" }, "Repositories Workbench knows"),
      h("ul", { class: "rp-klist" }, rows.slice(0, 8).map((r) => h("li", null, h("button", {
        class: "rp-k" + (opts.current === r.path ? " on" : ""), type: "button", title: r.path,
        onclick: () => { input.value = r.path; submit(); },
      },
      h("b", null, r.name || base(r.path)),
      h("span", { class: "rp-kp" }, r.path),
      h("span", { class: "rp-km" }, [r.branch, r.agents_running ? `${r.agents_running} running` : r.agents ? `${r.agents} agent${r.agents === 1 ? "" : "s"} before` : "no agent yet", r.last_ms ? fmtAgo(r.last_ms) : null].filter(Boolean).join(" · ")))))));
  }

  // A page that lists the projects itself leaves this list out (one list, not two).
  if (opts.known !== false) loadKnown();
  const form = h("form", { class: "lv-card rp" + (opts.compact ? " compact" : ""), onsubmit: submit },
    h("h2", { class: "rp-h" }, opts.title || "Open a repository"),
    h("p", { class: "rp-why" }, opts.why || "Search, Map, Trace and Library read a repository's code and history. No agent is needed: point Workbench at a git checkout."),
    h("div", { class: "rp-row" }, h("div", { class: "rp-box" }, input, list), go),
    h("div", { class: "rp-hint" }, h("kbd", null, "Tab"), " completes a folder · ", h("kbd", null, "↑"), h("kbd", null, "↓"), " pick · git checkouts come first"),
    err, opts.known === false ? null : known);
  queueMicrotask(() => { if (opts.focus !== false && form.isConnected) input.focus({ preventScroll: true }); });
  return form;
}

/** A button naming the repository a screen reads; a click opens the picker
 *  in a popover, to read another. */
export function repoSwitch(ctx, repo) {
  const btn = h("button", { class: "rp-switch", type: "button", title: repo ? `${repo}: read another repository` : "Open a repository", "aria-haspopup": "dialog" },
    h("span", { class: "rp-sw-i", "aria-hidden": "true" }, "⌂"), h("b", null, repo ? base(repo) : "Open a repository"), h("span", { class: "rp-sw-c", "aria-hidden": "true" }, "▾"));
  let pop = null, drop = null;
  const close = () => { pop?.remove(); pop = null; drop?.(); drop = null; document.removeEventListener("mousedown", outside, true); };
  const outside = (e) => { if (pop && !pop.contains(e.target) && !btn.contains(e.target)) close(); };
  btn.addEventListener("click", () => {
    if (pop) { close(); return; }
    const r = btn.getBoundingClientRect();
    pop = h("div", { class: "rp-pop", role: "dialog", "aria-label": "Open a repository", style: { top: Math.round(r.bottom + 6) + "px", left: Math.round(Math.max(8, Math.min(r.left, window.innerWidth - 540))) + "px" } },
      repoPicker(ctx, { current: repo, compact: true, onDone: close }));
    document.body.append(pop);
    drop = onEscape(close);
    document.addEventListener("mousedown", outside, true);
  });
  return btn;
}

useStyle("repos", `
.rp{max-width:720px;margin:28px auto;display:flex;flex-direction:column;gap:10px;padding:20px 22px}
.rp.compact{margin:0;max-width:none;box-shadow:none;border:0;padding:14px 16px}
.rp-h{margin:0;font-size:19px;font-weight:680;letter-spacing:-.01em}
.rp.compact .rp-h{font-size:15px}
.rp-why{margin:0;color:var(--text-2);line-height:1.5}
.rp-row{display:flex;gap:8px;align-items:flex-start}
.rp-box{position:relative;flex:1;min-width:0}
.rp-in{width:100%;box-sizing:border-box;height:36px;padding:0 10px;border:1px solid var(--line-2);border-radius:8px;background:var(--surface);color:var(--text);font:13px var(--mono)}
.rp-in:focus{outline:2px solid var(--focus);border-color:var(--accent)}
.rp-in[aria-invalid="true"]{border-color:var(--crit)}
.rp-row .btn{height:36px}
.rp-dirs{position:absolute;z-index:30;left:0;right:0;top:40px;margin:0;padding:4px;list-style:none;max-height:280px;overflow:auto;background:var(--surface);border:1px solid var(--line-2);border-radius:9px;box-shadow:var(--shadow-2)}
.rp-dirs[hidden]{display:none}
.rp-dir{display:flex;align-items:center;gap:8px;padding:5px 8px;border-radius:6px;cursor:pointer;font:12.5px var(--mono)}
.rp-dir:hover,.rp-dir.sel{background:var(--accent-soft)}
.rp-dpath{flex:1;min-width:0;overflow-wrap:anywhere}
.rp-dhead{color:var(--muted)}
.rp-git{flex:none;font:600 10.5px var(--sans);color:var(--good-ink);background:var(--good-soft);border-radius:999px;padding:1px 7px}
.rp-hint{color:var(--muted);font-size:11.5px}
.rp-hint kbd{font:11px var(--mono);border:1px solid var(--line-2);border-radius:4px;padding:0 4px;margin:0 1px}
.rp-err{color:var(--crit-ink);background:var(--crit-soft);border-radius:7px;padding:6px 9px}
.rp-err[hidden]{display:none}
.rp-kh{margin-top:6px;color:var(--muted);font-size:11px;font-weight:650;text-transform:uppercase;letter-spacing:.06em}
.rp-klist{list-style:none;margin:6px 0 0;padding:0;display:flex;flex-direction:column;gap:4px}
.rp-k{width:100%;display:grid;grid-template-columns:auto 1fr;gap:1px 10px;text-align:left;border:1px solid var(--line);border-radius:8px;background:var(--surface-2);padding:7px 10px;color:var(--text);font:inherit;cursor:pointer}
.rp-k:hover,.rp-k.on{border-color:var(--accent)}
.rp-k b{font-weight:650}
.rp-kp{font:11.5px var(--mono);color:var(--text-2);overflow-wrap:anywhere}
.rp-km{grid-column:1/-1;color:var(--muted);font-size:11.5px}
.rp-none{color:var(--muted);font-size:12px;margin-top:4px}
.rp-switch{display:inline-flex;align-items:center;gap:6px;height:28px;padding:0 10px;border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--text-2);font:inherit;font-size:12.5px;cursor:pointer;align-self:center}
.rp-switch:hover{border-color:var(--line-2);color:var(--text)}
.rp-switch b{font-weight:650;color:var(--text)}
.rp-sw-c{font-size:10px;color:var(--muted)}
.rp-pop{position:fixed;z-index:70;width:min(540px,calc(100vw - 16px));background:var(--surface);border:1px solid var(--line-2);border-radius:12px;box-shadow:var(--shadow-2)}
`);
