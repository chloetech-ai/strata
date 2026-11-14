// Quick search (SPEC.md, "Fourth build", row 1): `/` anywhere, or the top
// bar's search button, opens one box over the repository's code, its
// crawled docs and ChloeCtx's decisions (GET /api/search/quick). Enter
// opens a code hit in Library, a doc page in a new tab, a decision in its
// agent's story; Shift-Enter on code opens the Why card of that line.
//   install(ctx): adds the top bar's button; open(ctx): opens the box.
// ctx = { api, go(hash), openWhy(args), repo(): string, openDecisions?() }
import { h, useStyle, avatar, ctxChip, fmtAgo, later } from "./common.js";
import { openWith, tintLine, marksOf, icon, pathNode, queryWords } from "./library.js";
// Sixth build: no repository yet is a picker in the box, not an error (ui/levels/repos.js).
import { repoPicker, needsRepo } from "./repos.js";

const SCOPES = [["all", "All"], ["code", "Code"], ["docs", "Docs"], ["decisions", "Decisions"], ["history", "History"]];
const nameOf = (a) => (a && (a.name || a.id)) || "?";
const hostOf = (u) => { try { return new URL(u).host; } catch { return u; } };
const pathOfUrl = (u) => { try { return new URL(u).pathname; } catch { return ""; } };
let last = { q: "", scope: "all" };
let current = null;

/** Puts a search button in the top bar, before Decisions. */
export function install(ctx) {
  const bar = document.querySelector("header.top");
  if (!bar || bar.querySelector(".qs-btn")) return;
  const btn = h("button", { class: "btn qs-btn", title: "Search code, docs and decisions (/)", "aria-label": "Search", onclick: () => open(ctx) },
    icon("search"), h("span", { class: "wide-only qs-btn-t" }, "Search"), h("kbd", { class: "wide-only" }, "/"));
  const before = bar.querySelector("#dec-btn");
  if (before) bar.insertBefore(btn, before); else bar.append(btn);
}

/** Opens the box (or focuses it when it is open). */
export function open(ctx, { q } = {}) {
  if (current) { current.focus(); return current; }
  const S = { q: q ?? last.q, scope: last.scope, data: null, err: null, busy: false, sel: 0 };
  const input = h("input", { type: "search", placeholder: "Search code, docs and decisions…", value: S.q, "aria-label": "Search", spellcheck: "false", autocomplete: "off",
    oninput: () => { S.q = input.value; S.sel = 0; soon(); } });
  // No completion list here: the results below are the list (Lee and Priya: a
  // dropdown covered them, Tab only added a badge, Esc took the focus away).
  const scopes = h("div", { class: "qs-scopes", role: "tablist" });
  const list = h("div", { class: "qs-list", role: "listbox" });
  const foot = h("div", { class: "qs-foot" },
    h("span", null, h("kbd", null, "↑"), h("kbd", null, "↓"), " move"), h("span", null, h("kbd", null, "Enter"), " open"),
    h("span", null, h("kbd", null, "⇧"), h("kbd", null, "Enter"), " why"), h("span", null, h("kbd", null, "Tab"), " scope"),
    h("span", null, h("kbd", null, "Esc"), " close"),
    h("button", { class: "lv-link qs-lib", onclick: () => { openWith({ tab: "code", q: S.q }); close(); ctx.go("#/library/code"); } }, "Open Library →"));
  const panel = h("div", { class: "qs-panel", role: "dialog", "aria-modal": "true", "aria-label": "Quick search" },
    h("div", { class: "qs-in" }, icon("search"), input, h("span", { class: "qs-spin" }), h("kbd", null, "Esc")), scopes, list, foot);
  const back = h("div", { class: "qs-back", onmousedown: (e) => { if (e.target === back) close(); } }, panel);
  document.body.append(back);

  let req = 0;
  async function load() {
    const q = S.q.trim();
    last.q = S.q;
    if (!q || !(ctx.repo && ctx.repo())) { S.data = null; render(); return; }
    const r = ++req;
    S.busy = true; panel.classList.add("busy");
    try {
      const d = await ctx.api(`/api/search/quick?${new URLSearchParams({ repo: (ctx.repo && ctx.repo()) || "", q })}`);
      if (r !== req) return;
      S.data = d; S.err = null;
    } catch (e) { if (r === req) { S.err = e.message || String(e); S.noRepo = needsRepo(e); } }
    if (r === req) { S.busy = false; panel.classList.remove("busy"); }
    render();
  }
  const soon = later(load, 120);

  function items() {
    const d = S.data || {};
    const out = [];
    if (S.scope === "all" || S.scope === "code") for (const c of (d.code || []).slice(0, S.scope === "all" ? 5 : 12)) out.push({ kind: "code", c });
    if (S.scope === "all" || S.scope === "docs") for (const p of (d.docs || []).slice(0, S.scope === "all" ? 4 : 12)) out.push({ kind: "docs", p });
    if (S.scope === "all" || S.scope === "decisions") for (const x of (d.decisions || []).slice(0, S.scope === "all" ? 4 : 12)) out.push({ kind: "decisions", x });
    // The commits: Trace reads git for the words (Priya: search had no history).
    if ((S.scope === "all" || S.scope === "history") && S.q.trim()) out.push({ kind: "history" });
    return out;
  }

  function render() {
    const d = S.data;
    const count = (k) => (d ? (d[k] || []).length : null);
    scopes.replaceChildren(...SCOPES.map(([k, label]) => h("button", { role: "tab", "aria-selected": String(S.scope === k), class: S.scope === k ? "on" : "", tabindex: "-1",
      onclick: () => { S.scope = k; last.scope = k; S.sel = 0; render(); input.focus(); } }, label, k !== "all" && count(k) != null ? h("span", null, String(count(k))) : null)));
    const all = items();
    S.sel = Math.max(0, Math.min(all.length - 1, S.sel));
    const marks = marksOf(S.q);
    const groups = [];
    let i = 0;
    for (const [k, label] of SCOPES.slice(1)) {
      const these = all.filter((x) => x.kind === k);
      if (!these.length) continue;
      groups.push(h("div", { class: "qs-group" }, label, k === "decisions" ? ctxChip("ChloeCtx") : null));
      for (const it of these) groups.push(row(it, i++, marks));
    }
    if (!(ctx.repo && ctx.repo()) || (S.err && S.noRepo)) {
      // Searched before any repository: open one here, then the search runs.
      if (!list.querySelector(".rp")) list.replaceChildren(repoPicker(ctx, { compact: true, focus: false, why: "Search reads a repository's code, the docs of its libraries and ChloeCtx's decisions. Open the git checkout to search; no agent is needed.", onDone: () => { S.err = null; S.noRepo = false; input.focus(); load(); } }));
    }
    else if (S.err) list.replaceChildren(h("div", { class: "qs-empty bad" }, `The search failed: ${S.err}`));
    else if (!S.q.trim()) list.replaceChildren(h("div", { class: "qs-empty" }, "A symbol (", h("code", { class: "lv-code" }, "cartTotal"), "), a few words, an error message. Code and docs come from the Library's index; decisions from ChloeCtx."));
    else if (!d) list.replaceChildren(h("div", { class: "qs-empty" }, "Searching…"));
    else if (!all.length) list.replaceChildren(h("div", { class: "qs-empty" }, `Nothing for “${S.q.trim()}”${S.scope === "all" ? "" : " here"}.`, ...(d.missing || []).map((m) => h("div", { class: "qs-miss" }, m))));
    else list.replaceChildren(...groups, ...(d.missing || []).map((m) => h("div", { class: "qs-miss" }, m)));
    list.querySelector(".qs-item.sel")?.scrollIntoView({ block: "nearest" });
  }

  function row(it, i, marks) {
    const sel = i === S.sel;
    const attrs = { class: "qs-item" + (sel ? " sel" : ""), role: "option", "aria-selected": String(sel), onmousemove: () => { if (S.sel !== i) { S.sel = i; render(); } }, onclick: (e) => act(it, e.shiftKey) };
    if (it.kind === "code") {
      const c = it.c;
      const lines = c.text.split("\n");
      const at = Math.max(0, lines.findIndex((l) => { if (!marks) return false; marks.lastIndex = 0; return marks.test(l); }));
      return h("div", attrs, h("span", { class: "qs-ic code" }, icon("code", "i sm")),
        h("div", { class: "qs-main" },
          h("div", { class: "qs-t" }, c.symbol ? h("b", { class: "qs-sym" }, c.symbol) : null, pathNode(c.path), h("span", { class: "qs-ln" }, `:${c.start_line}–${c.end_line}`),
            c.edited_by ? h("span", { class: "qs-who" }, avatar(c.edited_by, 14), `${nameOf(c.edited_by)}'s edit`) : null),
          h("div", { class: "qs-code" }, h("span", { class: "qs-no" }, String(c.start_line + at)), h("span", null, tintLine(lines[at].trim(), c.lang, marks)))),
        h("span", { class: "qs-kind" }, c.lang));
    }
    if (it.kind === "docs") {
      const p = it.p;
      return h("div", attrs, h("span", { class: "qs-ic docs" }, icon("doc", "i sm")),
        h("div", { class: "qs-main" }, h("div", { class: "qs-t" }, h("b", null, p.title), h("span", { class: "qs-host" }, `${hostOf(p.url)}${pathOfUrl(p.url)}`)),
          h("div", { class: "qs-sub" }, (p.text || "").slice(0, 150))),
        h("span", { class: "qs-kind" }, p.library?.name || "docs"));
    }
    if (it.kind === "history") {
      return h("div", attrs, h("span", { class: "qs-ic code" }, icon("refresh", "i sm")),
        h("div", { class: "qs-main" }, h("div", { class: "qs-t" }, h("b", null, `Trace “${S.q.trim()}” through git`)),
          h("div", { class: "qs-sub" }, "The commits on every branch that changed it, with when, who and why.")),
        h("span", { class: "qs-kind" }, "commits"));
    }
    const x = it.x;
    return h("div", attrs, h("span", { class: "qs-ic dec" }, h("i")),
      h("div", { class: "qs-main" }, h("div", { class: "qs-t qs-dec" }, x.text),
        h("div", { class: "qs-sub" }, x.agent ? [avatar(x.agent, 14), " ", nameOf(x.agent), " · "] : "another session · ", x.ts ? fmtAgo(x.ts) : "", (x.files || []).length ? [" · ", h("code", { class: "lv-code" }, x.files[0])] : null)),
      h("span", { class: "qs-kind" }, "decision"));
  }

  function act(it, why) {
    if (!it) return;
    if (it.kind === "code") {
      if (why && ctx.openWhy) { close(); ctx.openWhy({ agent: it.c.tree !== "main" ? it.c.tree : null, path: it.c.path, line: it.c.start_line }); return; }
      openWith({ tab: "code", q: S.q, focus: it.c.id });
      close();
      ctx.go("#/library/code");
    } else if (it.kind === "docs") {
      window.open(it.p.url, "_blank", "noopener,noreferrer");
    } else if (it.kind === "history") {
      const q = S.q.trim();
      close();
      ctx.go(`#/debug/code?q=${encodeURIComponent(q)}`);
    } else {
      close();
      if (it.x.agent) ctx.go(`#/story/${it.x.agent.id}`);
      else if (ctx.openDecisions) ctx.openDecisions();
    }
  }

  function onKey(e) {
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(); return; }
    if (e.target.closest && e.target.closest(".rp")) return; // the repository picker's own keys
    const all = items();
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); S.sel = (S.sel + (e.key === "ArrowDown" ? 1 : -1) + all.length) % Math.max(1, all.length); render(); return; }
    if (e.key === "Enter") { e.preventDefault(); act(all[S.sel], e.shiftKey); return; }
    if (e.key === "Tab") {
      e.preventDefault();
      const i = SCOPES.findIndex(([k]) => k === S.scope);
      S.scope = SCOPES[(i + (e.shiftKey ? -1 : 1) + SCOPES.length) % SCOPES.length][0];
      last.scope = S.scope; S.sel = 0; render();
    }
  }
  panel.addEventListener("keydown", onKey);

  function close() {
    soon.cancel();
    back.remove();
    current = null;
  }
  current = { focus: () => { input.focus(); input.select(); }, close };
  render();
  if (S.q.trim()) load();
  requestAnimationFrame(() => { input.focus(); input.select(); });
  return current;
}

useStyle("quicksearch", `
.qs-btn .qs-btn-t{color:var(--text-2)}
.qs-back{position:fixed;inset:0;z-index:70;background:color-mix(in srgb,var(--bg) 55%,transparent);backdrop-filter:blur(2px);display:flex;justify-content:center;align-items:flex-start;padding:10vh 16px 16px}
.qs-panel{width:min(760px,100%);max-height:76vh;display:flex;flex-direction:column;background:var(--surface);border:1px solid var(--line-2);border-radius:14px;box-shadow:var(--shadow-3);overflow:hidden;font-size:13px;color:var(--text)}
.qs-in{display:flex;align-items:center;gap:10px;padding:0 14px;height:54px;border-bottom:1px solid var(--line)}
.qs-in>svg{width:19px;height:19px;color:var(--muted)}
.qs-in input,.qs-in input:focus,.qs-in input:focus-visible{outline:none!important;border:0!important;box-shadow:none!important}
.qs-in input{flex:1;-webkit-appearance:none;appearance:none;min-width:0;border:0;outline:0;background:none;font:16px var(--mono);color:var(--text)}
.qs-in input::placeholder{font-family:var(--sans);color:var(--muted)}
.qs-in input::-webkit-search-cancel-button{display:none}
.qs-spin{width:13px;height:13px;border-radius:50%;border:2px solid transparent}
.qs-panel.busy .qs-spin{border-color:var(--line-2);border-top-color:var(--accent);animation:spin .8s linear infinite}
.qs-scopes{display:flex;gap:4px;padding:8px 12px;border-bottom:1px solid var(--line)}
.qs-scopes button{display:inline-flex;align-items:center;gap:6px;border:0;background:none;border-radius:7px;padding:4px 10px;font:inherit;font-size:12.5px;font-weight:600;color:var(--text-2);cursor:pointer}
.qs-scopes button span{font-weight:500;color:var(--muted);font-size:11.5px}
.qs-scopes button.on{background:var(--accent-soft);color:var(--accent)}.qs-scopes button.on span{color:inherit}
.qs-list{overflow:auto;padding:6px 0 8px;min-height:120px}
.qs-group{display:flex;align-items:center;gap:10px;padding:10px 16px 4px;font-size:11px;font-weight:700;color:var(--muted);text-transform:uppercase;letter-spacing:.06em}
.qs-group .lv-ctx{text-transform:none;letter-spacing:0}
.qs-item{display:flex;align-items:flex-start;gap:11px;padding:8px 16px;cursor:pointer;border-left:3px solid transparent}
.qs-item.sel{background:var(--accent-soft);border-left-color:var(--accent)}
.qs-ic{display:inline-grid;place-items:center;width:24px;height:24px;border-radius:7px;flex:none;margin-top:1px}
.qs-ic.code{background:color-mix(in srgb,var(--lane-1) 16%,transparent);color:var(--lane-1)}
.qs-ic.docs{background:color-mix(in srgb,var(--lane-3) 16%,transparent);color:var(--lane-3)}
.qs-ic.dec{background:color-mix(in srgb,var(--ctx) 14%,transparent)}
.qs-ic.dec i{width:8px;height:8px;border-radius:2px;background:var(--ctx);transform:rotate(45deg)}
.qs-main{flex:1;min-width:0;display:flex;flex-direction:column;gap:2px}
.qs-t{display:flex;align-items:baseline;gap:8px;min-width:0;white-space:nowrap;overflow:hidden}
.qs-t>b{font-weight:650}
.qs-sym{font-family:var(--mono);font-size:13px}
.qs-t .lb-path{font-size:12px}
.qs-ln{font:12px var(--mono);color:var(--muted);margin-left:-7px}
.qs-who{display:inline-flex;align-items:center;gap:5px;font-size:11px;font-weight:600;color:var(--good-ink);background:var(--good-soft);border-radius:999px;padding:1px 7px 1px 4px}
.qs-who .lv-av{border-radius:4px;font-size:8.5px}
.qs-host{font:11.5px var(--mono);color:var(--muted);overflow:hidden;text-overflow:ellipsis}
.qs-code{font:12px var(--mono);color:var(--text-2);white-space:pre;overflow:hidden;text-overflow:ellipsis}
.qs-code mark,.qs-sub mark{background:color-mix(in srgb,var(--warn) 30%,transparent);color:inherit;border-radius:3px}
.qs-no{color:var(--muted);opacity:.7;margin-right:10px}
.qs-sub{font-size:12px;color:var(--muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;display:flex;align-items:center;gap:3px}
.qs-sub .lv-av{border-radius:4px;font-size:8.5px}
.qs-dec{white-space:normal;color:var(--text);line-height:1.4}
.qs-kind{font-size:11px;color:var(--muted);flex:none;margin-top:4px}
.qs-empty{padding:22px 18px;color:var(--muted);line-height:1.6}.qs-empty.bad{color:var(--crit-ink)}
.qs-miss{padding:6px 16px;font-size:12px;color:var(--warn-ink)}
.qs-foot{display:flex;align-items:center;gap:14px;padding:8px 14px;border-top:1px solid var(--line);font-size:11.5px;color:var(--muted);background:var(--surface-2);flex-wrap:wrap}
.qs-foot span{display:inline-flex;align-items:center;gap:3px}
.qs-lib{margin-left:auto;font-size:12px}
@media (max-width:640px){.qs-back{padding-top:12px}.qs-foot span,.qs-in kbd{display:none}}
`);
