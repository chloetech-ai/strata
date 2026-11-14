// Hotspots (SPEC.md, "Fifth build: Hotspots"): the files and functions the
// agents keep correcting, redoing or breaking, scored from Workbench's own
// rows by `GET /api/hotspots` (src/hotspots.rs), with what ChloeCtx knows of
// each (◆). Three additions to existing screens, each a wrapper the screen
// applies to its mount in one place:
//   withHeat(mount)      Map: a "Heat" toggle in the legend; files and
//                        folders tinted by score, the tooltip says why, and a
//                        card of the hottest files on the side.
//   withHotspots(mount)  Health: a fourth tab, Hotspots (#/health/hotspots,
//                        key 4): the ranked list with its reasons as chips and
//                        a sparkline of churn, and a board per hotspot: why,
//                        the evidence on a timeline (Story, test, error,
//                        incident, commit), what ChloeCtx knows, what the next
//                        agent to edit it is told, and Add rule, Pin a test,
//                        Route to a stronger model.
//   withHotBadges(mount) Mission: a small badge on the card of an agent that
//                        works on a hotspot now.
// The wrappers watch the screen's own DOM (it redraws itself) and add to it;
// they change nothing the screen draws.
import { h, useStyle, avatar, ctxChip, pill, empty, fmtAgo, fmtDay, toMs, tooltip, repoOf, routeParam, inlineCode, later } from "./common.js";

const enc = encodeURIComponent;
const base = (p) => String(p || "").split("/").pop();
const dirOf = (p) => (String(p).includes("/") ? String(p).slice(0, String(p).lastIndexOf("/") + 1) : "");
const under = (p, dir) => !dir || p === dir || p.startsWith(dir + "/");
const typing = (el) => el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName));
const store = (k, v) => { try { if (v === undefined) return localStorage.getItem(k); localStorage.setItem(k, v); } catch {} return null; };
const MAX = 30;
/** 0..1 of a score: 30 and above is as hot as it gets. */
const heatOf = (score) => Math.max(0, Math.min(1, (score || 0) / MAX));
/** The heat colour at an alpha: amber to red. */
const heatColor = (score, alpha = 1) => {
  const p = Math.round(heatOf(score) * 100);
  const c = `color-mix(in srgb, var(--crit) ${p}%, var(--warn))`;
  return alpha >= 1 ? c : `color-mix(in srgb, ${c} ${Math.round(alpha * 100)}%, transparent)`;
};
const KIND = {
  re_edits: ["re-edits", "re-edited", "warn"], agents: ["agents", "agents", "mut"], reverts: ["reverts", "reverted", "crit"],
  test_fails: ["test fails", "failed tests", "crit"], flaky: ["flaky", "flaky", "warn"], errors: ["errors", "runtime errors", "crit"],
  steers: ["corrections", "corrected by you", "acc"], questions: ["questions", "asked you", "acc"], incidents: ["incidents", "incidents", "crit"],
  commits: ["commits", "commits", "mut"], fixes: ["fix commits", "fix commits", "warn"],
};
const EV = {
  edit: ["Edit", "var(--accent)"], revert: ["Revert", "var(--crit)"], test_fail: ["Test failed", "var(--crit)"], flaky: ["Flaky", "var(--warn)"],
  error: ["Runtime error", "var(--crit)"], steer: ["You said", "var(--good)"], prompt: ["Prompted again", "var(--good)"], question: ["Asked you", "var(--ctx)"],
  incident: ["Incident", "var(--warn)"], record: ["ChloeCtx", "var(--ctx)"],
  commit: ["Commit", "var(--muted)"], fix: ["Fix commit", "var(--warn)"],
};
const chipText = (r) => {
  const n = r.n;
  switch (r.kind) {
    case "re_edits": return `${n} re-edit${n === 1 ? "" : "s"}`;
    case "agents": return `${n} agents`;
    case "reverts": return `${n} revert${n === 1 ? "" : "s"}`;
    case "test_fails": return `${n} test fail${n === 1 ? "" : "s"}`;
    case "flaky": return `${n} flaky test${n === 1 ? "" : "s"}`;
    case "errors": return `${n} error${n === 1 ? "" : "s"}`;
    case "steers": return `${n} correction${n === 1 ? "" : "s"}`;
    case "questions": return `${n} question${n === 1 ? "" : "s"}`;
    case "incidents": return `${n} incident${n === 1 ? "" : "s"}`;
    case "commits": return `${n} commit${n === 1 ? "" : "s"}`;
    case "fixes": return `${n} fix commit${n === 1 ? "" : "s"}`;
    default: return `${n} ${r.kind}`;
  }
};
const reasonChip = (r) => h("span", { class: `hs-rc t-${(KIND[r.kind] || [])[2] || "mut"}`, title: r.text }, chipText(r));

// ------------------------------------------------------------------ data
const cache = new Map();
/** `GET /api/hotspots` for a repository, kept 15 s and shared by the wrappers. */
function load(ctx, { days = 14, force = false } = {}) {
  const repo = repoOf(ctx);
  const key = `${repo}|${days}`;
  const had = cache.get(key);
  if (had && !force && Date.now() - had.at < 15000) return had.p;
  const q = new URLSearchParams({ days: String(days), limit: "100" });
  if (repo) q.set("repo", repo);
  const p = ctx.api(`/api/hotspots?${q}`);
  cache.set(key, { at: Date.now(), p });
  p.catch(() => cache.delete(key));
  return p;
}

/** Bars of edits per day: a sparkline. */
export function sparkline(churn, { w = 84, hgt = 22, score = 20 } = {}) {
  const vals = (churn || []).map((c) => c[1]);
  const max = Math.max(1, ...vals);
  const n = Math.max(1, vals.length);
  const bw = w / n;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", w); svg.setAttribute("height", hgt); svg.setAttribute("viewBox", `0 0 ${w} ${hgt}`);
  svg.setAttribute("class", "hs-spark"); svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", `edits per day: ${vals.join(", ")}`);
  vals.forEach((v, i) => {
    const r = document.createElementNS(svg.namespaceURI, "rect");
    const bh = v ? Math.max(2.5, (v / max) * (hgt - 2)) : 1.5;
    r.setAttribute("x", (i * bw + 0.8).toFixed(1)); r.setAttribute("width", Math.max(1, bw - 1.6).toFixed(1));
    r.setAttribute("y", (hgt - bh).toFixed(1)); r.setAttribute("height", bh.toFixed(1)); r.setAttribute("rx", 1);
    r.setAttribute("fill", v ? heatColor(score) : "var(--line-2)");
    if (!v) r.setAttribute("opacity", ".6");
    const t = document.createElementNS(svg.namespaceURI, "title");
    t.textContent = `${fmtDay(churn[i][0])}: ${v} edit${v === 1 ? "" : "s"}`;
    r.append(t);
    svg.append(r);
  });
  return svg;
}

function scoreBadge(x, big) {
  return h("span", { class: "hs-score" + (big ? " big" : ""), style: { "--hs": heatColor(x.score) }, title: `score ${x.score} (${x.level})` },
    h("b", null, fmtScore(x.score)), big ? h("span", null, x.level) : null);
}
const ago = (t) => (Date.now() - toMs(t) > 36 * 3600e3 ? fmtDay(t) : fmtAgo(t));
const fmtScore = (s) => (s >= 10 ? String(Math.round(s)) : (Math.round(s * 10) / 10).toString());

// =============================================================== the Map
/** Wraps the Map screen's mount: the heat layer. */
export function withHeat(mountMap) {
  return function mount(root, ctx) {
    const view = mountMap(root, ctx);
    const el = root.lastElementChild;
    const S = { on: store("wb.map.heat") === "1" || /[?&]heat=1\b/.test(location.search + location.hash), data: null, err: null, byFile: new Map(), byDir: new Map() };
    let alive = true, frame = 0, busy = false;
    const tip = tooltip();

    async function fetchHeat(force) {
      try {
        S.data = await load(ctx, { force });
        S.err = null;
        S.byFile.clear(); S.byDir.clear();
        for (const x of S.data.hotspots || []) {
          const had = S.byFile.get(x.file);
          if (!had || had.score < x.score) S.byFile.set(x.file, x);
          for (let d = dirOf(x.file).replace(/\/$/, ""); ; d = dirOf(d).replace(/\/$/, "")) {
            const g = S.byDir.get(d) || { score: 0, n: 0, top: null };
            g.n++;
            if (x.score > g.score) { g.score = x.score; g.top = x; }
            S.byDir.set(d, g);
            if (!d) break;
          }
        }
      } catch (e) { S.err = e.message || String(e); }
      schedule();
    }

    function schedule() { cancelAnimationFrame(frame); frame = requestAnimationFrame(apply); }
    const mo = new MutationObserver(() => { if (!busy) schedule(); });
    mo.observe(el, { childList: true, subtree: true });

    function apply() {
      if (!alive || !el.isConnected) return;
      busy = true;
      try {
        const legend = el.querySelector(".mp-legend");
        if (legend && !legend.querySelector(".hs-toggle")) {
          legend.append(h("button", {
            class: "hs-toggle", "aria-pressed": "false", title: "Tint files by how often agents had to correct, redo or fix them (h)",
            onclick: () => toggle(),
          }, h("i", { class: "hs-sw" }), "Heat"));
        }
        const t = legend?.querySelector(".hs-toggle");
        if (t) {
          t.setAttribute("aria-pressed", S.on ? "true" : "false");
          t.classList.toggle("on", S.on);
          const n = S.data ? (S.data.hotspots || []).filter((x) => x.level !== "mild").length : 0;
          let c = t.querySelector(".hs-tn");
          if (S.on && n) { if (!c) t.append(c = h("b", { class: "hs-tn" })); c.textContent = String(n); } else c?.remove();
        }
        const tm = el.querySelector(".mp-tm");
        if (tm) {
          tm.classList.toggle("hs-on", S.on);
          for (const c of tm.querySelectorAll("[data-path]")) paint(c);
        }
        const side = el.querySelector(".mp-side");
        const card = side?.querySelector(".hs-mcard");
        if (side && S.on && S.data) { if (!card) side.prepend(sideCard()); }
        else card?.remove();
      } finally { busy = false; }
    }

    function paint(c) {
      const p = c.dataset.path;
      const folder = c.dataset.folder === "1";
      const x = S.on ? (folder ? S.byDir.get(p)?.top && S.byDir.get(p) : S.byFile.get(p)) : null;
      c.classList.toggle("hs-h", !!x);
      c.querySelector(":scope > .hs-hb")?.remove();
      if (!x) { c.style.removeProperty("--hs"); c.style.removeProperty("--hs-a"); return; }
      c.style.setProperty("--hs", heatColor(x.score));
      c.style.setProperty("--hs-a", heatColor(x.score, folder ? 0.04 + 0.07 * heatOf(x.score) : 0.18 + 0.5 * heatOf(x.score)));
      if (!folder && c.offsetWidth > 46 && c.offsetHeight > 30) c.append(h("span", { class: "hs-hb", "aria-hidden": "true" }, fmtScore(x.score)));
    }

    function toggle(v = !S.on) {
      S.on = v;
      store("wb.map.heat", v ? "1" : "0");
      if (v && !S.data) fetchHeat();
      tip.hide();
      schedule();
    }

    function sideCard() {
      const list = (S.data.hotspots || []).slice(0, 6);
      return h("section", { class: "lv-card hs-mcard" },
        h("div", { class: "lv-card-head" },
          h("h2", { class: "lv-h2 hs-mh" }, h("i", { class: "hs-sw" }), ` ${(S.data.hotspots || []).length} hotspots`),
          h("span", { class: "lv-sub" }, `last ${S.data.window?.days || 14} days`)),
        list.length ? h("div", { class: "hs-ml" }, list.map((x) => h("button", {
          class: "hs-mr", onclick: () => ctx.go(`#/health/hotspots?h=${enc(x.id)}`),
          onmouseenter: () => el.querySelectorAll(`[data-path="${CSS.escape(x.file)}"]`).forEach((c) => c.classList.add("hs-flash")),
          onmouseleave: () => el.querySelectorAll(".hs-flash").forEach((c) => c.classList.remove("hs-flash")),
        },
          scoreBadge(x),
          h("span", { class: "hs-mt" },
            h("span", { class: "hs-mf" }, h("span", { class: "hs-dir" }, dirOf(x.file)), h("b", null, base(x.file)), x.symbol ? h("span", { class: "hs-sym" }, ` ${x.symbol}`) : null),
            h("span", { class: "hs-chips" }, (x.reasons || []).slice(0, 3).map(reasonChip))),
          sparkline(x.churn, { w: 56, hgt: 18, score: x.score })))) : h("div", { class: "lv-sub" }, "Nothing is hot: no file needed correcting more than once."),
        S.data.missing?.length ? h("div", { class: "lv-sub hs-miss" }, S.data.missing[0]) : null,
        h("button", { class: "lv-link hs-all", onclick: () => ctx.go("#/health/hotspots") }, "All hotspots in Health →"));
    }

    // Why it is hot: added to the map's own tooltip.
    function why(e) {
      if (!S.on) return;
      const t = e.target.closest && e.target.closest("[data-path]");
      if (!t || !el.contains(t)) return;
      const p = t.dataset.path, folder = t.dataset.folder === "1";
      const x = folder ? S.byDir.get(p)?.top : S.byFile.get(p);
      if (!x) return;
      const box = [...document.querySelectorAll(".lv-tip")].find((b) => !b.hidden && b !== tipEl());
      if (!box || box.querySelector(".hs-tip")) return;
      const g = folder ? S.byDir.get(p) : null;
      box.append(h("div", { class: "hs-tip", style: { "--hs": heatColor(x.score) } },
        h("div", { class: "hs-tip-h" }, h("i", { class: "hs-sw" }), folder ? `${g.n} hotspot${g.n === 1 ? "" : "s"} here · hottest ${base(x.file)}` : `Hotspot · score ${fmtScore(x.score)}`),
        h("div", { class: "hs-tip-t" }, x.summary || (x.reasons || []).map((r) => r.text).join("; ")),
        folder ? null : h("div", { class: "hs-chips" }, (x.reasons || []).slice(0, 5).map(reasonChip), (x.records || []).slice(0, 2).map((r) => ctxChip(r.via === "rule" ? "rule" : r.kind, r.text))),
        h("div", { class: "lv-sub" }, "The evidence is in Health › Hotspots (the side card's rows).")));
      const r = box.getBoundingClientRect();
      if (r.bottom > innerHeight - 8) box.style.top = Math.max(8, innerHeight - 8 - r.height) + "px";
    }
    const tipEl = () => null;
    el.addEventListener("mousemove", why);
    el.addEventListener("focusin", (e) => requestAnimationFrame(() => why(e)));
    const onKey = (e) => {
      if (e.key !== "h" || e.metaKey || e.ctrlKey || e.altKey || typing(e.target) || !el.isConnected) return;
      e.preventDefault();
      toggle();
    };
    document.addEventListener("keydown", onKey);
    if (S.on) fetchHeat(); else schedule();
    const poll = setInterval(() => { if (S.on) fetchHeat(); }, 30000);

    return {
      update(state) { view.update(state); },
      unmount() { alive = false; mo.disconnect(); clearInterval(poll); cancelAnimationFrame(frame); document.removeEventListener("keydown", onKey); tip.remove(); view.unmount(); },
    };
  };
}

// ============================================================ Health's tab
/** Wraps the Health screen's mount: a fourth tab, Hotspots. */
export function withHotspots(mountHealth) {
  return function mount(root, ctx) {
    const view = mountHealth(root, ctx);
    const el = root.lastElementChild;
    const hashSel = () => (location.hash.match(/[?&]h=([^&]+)/) || [])[1];
    const S = {
      active: routeParam(ctx, "agent") === "hotspots" || /^#\/health\/hotspots/.test(location.hash),
      days: 14, data: null, err: null, sel: hashSel() ? decodeURIComponent(hashSel()) : null, boards: new Map(),
      busy: new Set(), done: new Map(), ruleText: new Map(), testPick: new Map(), modelPick: new Map(), filter: "all",
    };
    const panel = h("div", { class: "hs-panel" });
    let alive = true, busy = false;
    const syncSoon = later(() => sync(), 0);

    const agentOf = (id) => (S.data?.agents || []).find((a) => a.id === id) || (ctx.state?.agents || []).find((a) => a.id === id) || { id, name: id };

    async function fetchList(force) {
      try {
        S.data = await load(ctx, { days: S.days, force });
        S.err = null;
        const list = S.data.hotspots || [];
        if (!S.sel || !list.some((x) => x.id === S.sel)) S.sel = list[0]?.id || null;
        if (S.sel) await fetchBoard(S.sel);
      } catch (e) { S.err = e.message || String(e); }
      draw();
    }
    async function fetchBoard(id, force) {
      if (S.boards.has(id) && !force) return;
      try { S.boards.set(id, await ctx.api(`/api/hotspots/${enc(id)}`)); }
      catch (e) { S.boards.set(id, { error: e.message || String(e) }); }
    }

    function activate(on) {
      if (S.active === on) return;
      S.active = on;
      if (on) {
        try { history.replaceState(history.state, "", `#/health/hotspots${S.sel ? `?h=${enc(S.sel)}` : ""}`); } catch {}
        if (!S.data) fetchList();
      }
      sync();
    }

    function sync() {
      if (!alive || !el.isConnected) return;
      busy = true;
      try {
        const tabs = el.querySelector(".hl-tabs");
        if (tabs) {
          let mine = tabs.querySelector(".hs-tab");
          if (!mine) {
            mine = h("button", { role: "tab", class: "hs-tab", "data-key": "tab:hotspots", onclick: () => activate(true), title: "Files and functions the agents keep correcting (4)" }, "Hotspots");
            tabs.append(mine);
          }
          const hot = (S.data?.hotspots || []).filter((x) => x.level === "hot").length;
          let n = mine.querySelector(".hl-tn");
          if (hot) { if (!n) mine.append(n = h("b", { class: "hl-tn crit" })); n.textContent = String(hot); } else n?.remove();
          mine.classList.toggle("on", S.active);
          mine.setAttribute("aria-selected", S.active ? "true" : "false");
          for (const b of tabs.querySelectorAll("button:not(.hs-tab)")) {
            if (S.active) { b.classList.remove("on"); b.setAttribute("aria-selected", "false"); }
          }
        }
        for (const c of [...el.children]) {
          if (c === panel || c.classList.contains("hl-head") || c.tagName === "HEADER") continue;
          c.style.display = S.active ? "none" : "";
        }
        if (S.active && panel.parentNode !== el) el.append(panel);
        if (!S.active && panel.parentNode) panel.remove();
      } finally { busy = false; }
    }
    const mo = new MutationObserver(() => { if (!busy) syncSoon(); });
    mo.observe(el, { childList: true });
    // Health's own tabs (a click or 1 2 3) leave Hotspots; 4 opens it.
    const onClick = (e) => { const b = e.target.closest && e.target.closest(".hl-tabs button"); if (b && !b.classList.contains("hs-tab")) { S.active = false; } };
    el.addEventListener("click", onClick, true);
    const onKey = (e) => {
      if (!el.isConnected || e.metaKey || e.ctrlKey || e.altKey || typing(e.target)) return;
      if (e.key === "4") { e.preventDefault(); activate(true); }
      else if (e.key === "1" || e.key === "2" || e.key === "3") S.active = false;
      else if (S.active && (e.key === "j" || e.key === "k") && !document.activeElement?.closest?.(".hs-panel")) {
        const list = S.data?.hotspots || [];
        const i = list.findIndex((x) => x.id === S.sel);
        const next = list[Math.max(0, Math.min(list.length - 1, i + (e.key === "j" ? 1 : -1)))];
        if (next) { e.preventDefault(); pick(next.id); }
      }
    };
    document.addEventListener("keydown", onKey, true);

    async function pick(id) {
      S.sel = id;
      try { history.replaceState(history.state, "", `#/health/hotspots?h=${enc(id)}`); } catch {}
      draw();
      if (!S.boards.has(id)) { await fetchBoard(id); draw(); }
    }

    // ------------------------------------------------------------ drawing
    function draw() {
      if (!alive) return;
      const had = document.activeElement && panel.contains(document.activeElement) ? document.activeElement.dataset.key : null;
      panel.replaceChildren(...[body()].flat().filter(Boolean));
      if (had) panel.querySelector(`[data-key="${CSS.escape(had)}"]`)?.focus({ preventScroll: true });
      sync();
    }

    function body() {
      if (!S.data) return S.err ? h("section", { class: "lv-card" }, h("div", { class: "err" }, S.err), h("button", { class: "btn sm", onclick: () => fetchList(true) }, "Try again")) : empty("Scoring the files…");
      const all = S.data.hotspots || [];
      // A filter's kinds: Tests is failing and flaky tests both.
      const kinds = { test_fails: ["test_fails", "flaky"] }[S.filter] || [S.filter];
      const shown = all.filter((x) => S.filter === "all" || (S.filter === "hot" ? x.level === "hot" : (x.reasons || []).some((r) => kinds.includes(r.kind))));
      // The pane shows one of the list as filtered, never one it hides.
      const sel = shown.find((x) => x.id === S.sel) || shown[0];
      if (sel && sel.id !== S.sel) setTimeout(() => pick(sel.id), 0);
      return [
        stats(),
        h("div", { class: "hs-grid" },
          h("aside", { class: "hs-list" },
            h("div", { class: "hs-lh" }, h("h2", { class: "lv-h" }, "Ranked", h("span", { class: "n" }, `· ${shown.length}`)),
              h("div", { class: "hs-filters", role: "group", "aria-label": "Show" }, [["all", "All"], ["hot", "Hot"], ["reverts", "Reverts"], ["test_fails", "Tests"], ["steers", "Corrected"]].map(([k, l]) =>
                h("button", { class: "hs-f" + (S.filter === k ? " on" : ""), "aria-pressed": S.filter === k ? "true" : "false", onclick: () => { S.filter = k; draw(); } }, l)))),
            shown.length ? shown.map((x) => row(x, all.indexOf(x) + 1)) : h("div", { class: "hs-calm" }, all.length ? "None of these here." : [h("b", null, "Nothing is hot."), " No file was corrected, reverted or broken more than once in this window."])),
          sel ? board(sel) : h("section", { class: "lv-card hs-board" }, empty(all.length ? (shown.length ? "Pick a hotspot." : "No hotspot of this kind. Pick another filter.") : "When agents keep re-editing, reverting or breaking a file, it shows here with the evidence."))),
      ];
    }

    function stats() {
      const t = S.data.totals || {};
      const tile = (n, label, sub, tone) => h("div", { class: "hs-tile" }, h("b", { class: tone ? `t-${tone}` : "" }, String(n ?? 0)), h("span", { class: "hs-tl" }, label), h("span", { class: "lv-sub" }, sub));
      // Sixth build: every count is the sum of the files' own counts, so the
      // tiles add up with the list (hotspots or not).
      const levels = [t.hot ? `${t.hot} hot` : null, t.warm ? `${t.warm} warm` : null, t.mild ? `${t.mild} mild` : null].filter(Boolean).join(" · ") || "none over the bar";
      return h("section", { class: "lv-card hs-stats" },
        // Its colour is its hottest level's: red for hot, amber for warm, plain for mild.
        tile(t.hotspots, t.hotspots === 1 ? "hotspot" : "hotspots", `${levels} · of ${t.files || 0} files with signals`, t.hot ? "crit" : t.warm ? "warn" : null),
        tile(t.edits, "agent edits", `in the last ${S.data.window?.days || S.days} days`),
        tile(t.commits, t.commits === 1 ? "commit" : "commits", `${t.fixes ? `${t.fixes} of them fixes · ` : ""}from git log, on every branch, each counted once`),
        tile(t.reverts, "reverts", "revert commits and edits put back", "warn"),
        tile(t.test_fails, "test failures", "after an edit, or of a test file's own tests", "warn"),
        tile(t.steers, "corrections", "your steers, prompts mid-work and permissions you refused", "acc"),
        h("div", { class: "hs-win" },
          h("div", { class: "hs-filters" }, [7, 14, 30].map((d) => h("button", { class: "hs-f" + (S.days === d ? " on" : ""), onclick: () => { S.days = d; S.boards.clear(); fetchList(); } }, `${d} d`))),
          h("span", { class: "lv-sub" }, S.data.updated_ms ? `scored ${fmtAgo(S.data.updated_ms)}` : "scored now"),
          h("button", { class: "btn sm ghost", onclick: async () => { try { await ctx.api("/api/hotspots/refresh", { method: "POST", body: { repo: S.data.repo } }); } catch {} S.boards.clear(); fetchList(true); } }, "Score again")));
    }

    function row(x, rank) {
      const on = x.id === S.sel;
      return h("button", {
        class: "hs-row" + (on ? " on" : ""), "data-key": `h:${x.id}`, "aria-pressed": on ? "true" : "false", style: { "--hs": heatColor(x.score) },
        onclick: () => pick(x.id),
      },
        h("i", { class: "hs-heat" }),
        h("span", { class: "hs-rank" }, String(rank)),
        h("span", { class: "hs-rb" },
          h("span", { class: "hs-rf" }, h("span", { class: "hs-dir" }, dirOf(x.file)), h("b", null, base(x.file))),
          x.symbol ? h("span", { class: "hs-rs" }, h("span", { class: "hs-kind" }, x.symbol_kind || "fn"), x.symbol) : null,
          h("span", { class: "hs-chips" }, (x.reasons || []).slice(0, 4).map(reasonChip),
            (x.records || []).length ? ctxChip(`${x.records.length}`, (x.records || []).map((r) => r.text).join("\n")) : null),
          h("span", { class: "hs-rm" }, h("span", { class: "hs-avs" }, (x.agents || []).slice(0, 3).map((id) => avatar(agentOf(id), 15))), h("span", null, `last ${ago(x.last_ms)}`))),
        h("span", { class: "hs-rr" }, scoreBadge(x), sparkline(x.churn, { w: 70, hgt: 20, score: x.score })));
    }

    function board(x) {
      const d = S.boards.get(x.id);
      const hx = d?.hotspot || x;
      const q = `why is ${hx.file} changing so often`;
      return h("section", { class: "lv-card hs-board", style: { "--hs": heatColor(hx.score) } },
        h("div", { class: "hs-bh" },
          h("div", { class: "hs-bt" },
            h("div", { class: "hs-bf" }, h("span", { class: "hs-dir" }, dirOf(hx.file)), h("b", null, base(hx.file)), hx.symbol ? h("span", { class: "hs-sym" }, ` › ${hx.symbol}${hx.line ? `:${hx.line}` : ""}`) : null),
            h("p", { class: "hs-sum" }, inlineCode(sentence(hx)))),
          scoreBadge(hx, true)),
        h("div", { class: "hs-acts" },
          h("button", { class: "btn sm", onclick: () => ctx.go(`#/trace?q=${enc(q)}`), title: "Trace: when and why it changed, from git, the edits and ChloeCtx" }, "Trace: why is it changing so often?"),
          h("button", { class: "btn sm ghost", onclick: () => { store("wb.map.heat", "1"); ctx.go("#/map"); } }, "Show on the map"),
          (hx.agents || []).slice(0, 2).map((id) => h("button", { class: "btn sm ghost", onclick: () => ctx.go(`#/story/${id}`) }, avatar(agentOf(id), 15), ` ${agentOf(id).name}'s story`))),
        h("div", { class: "hs-bgrid" },
          h("div", { class: "hs-col" },
            section("Why it is hot", reasons(hx)),
            section("Evidence", d ? (d.error ? h("div", { class: "err" }, d.error) : timeline(d.evidence || [])) : empty("Reading the evidence…"))),
          h("div", { class: "hs-col" },
            section("Churn · edits per day", h("div", { class: "hs-churn" }, sparkline(hx.churn, { w: 300, hgt: 40, score: hx.score }),
              h("div", { class: "hs-axis" }, h("span", null, fmtDay(hx.churn?.[0]?.[0])), h("span", null, "today")))),
            section("Do something about it", actions(hx)),
            section("What ChloeCtx knows", records(hx)),
            hx.symbols?.length ? section("Functions edited", h("ul", { class: "hs-syms" }, hx.symbols.map((s) => {
              const m = Math.max(...hx.symbols.map((y) => y.edits || 0), 1);
              return h("li", null, h("code", null, s.symbol), h("span", { class: "lv-sub" }, s.line ? `:${s.line}` : ""), h("span", { class: "hs-sbar" }, h("b", { style: { width: `${(100 * (s.edits || 0)) / m}%` } })), h("span", { class: "hs-sn" }, `${s.edits}`));
            }))) : null,
            section("Next agent to edit it is told", told(hx)))));
    }

    function sentence(x) {
      const s = x.summary || (x.reasons || []).map((r) => r.text).join("; ");
      return s.charAt(0).toUpperCase() + s.slice(1) + ".";
    }
    function section(title, ...kids) { return h("section", { class: "hs-sec" }, h("h3", { class: "lv-h" }, title), ...kids); }

    function reasons(x) {
      const pts = (x.reasons || []).map((r) => r.points || 0);
      const max = Math.max(1, ...pts);
      return h("ul", { class: "hs-why" }, (x.reasons || []).map((r) => h("li", null,
        reasonChip(r),
        h("span", { class: "hs-wt" }, r.text),
        r.points != null ? h("span", { class: "hs-pts", title: `${r.points} of the score's ${x.score} points` }, h("i", { style: { width: `${(100 * r.points) / max}%` } }), `+${fmtScore(r.points)}`) : null)));
    }

    function timeline(items) {
      if (!items.length) return h("div", { class: "lv-sub" }, "No evidence kept for this window.");
      return h("ol", { class: "hs-tl" }, items.map((e) => {
        const [label, color] = EV[e.kind] || [e.kind, "var(--muted)"];
        const a = e.agent ? agentOf(e.agent) : null;
        return h("li", { class: `hs-ev k-${e.kind}`, style: { "--c": color } },
          h("span", { class: "hs-when", title: new Date(e.ts_ms).toLocaleString() }, fmtWhen(e.ts_ms)),
          h("i", { class: "hs-dot" }),
          h("div", { class: "hs-eb" },
            h("div", { class: "hs-eh" }, h("span", { class: "hs-ek" }, label), a ? h("span", { class: "hs-ea" }, avatar(a, 14), a.name) : e.kind === "steer" ? h("span", { class: "hs-ea" }, avatar("you", 14), "you") : null,
              e.branch ? h("span", { class: "hs-br", title: "the branch this happened on" }, `on ${e.branch}`) : null),
            h("div", { class: "hs-et" }, inlineCode(e.text)),
            e.detail ? h("div", { class: "hs-ed lv-sub" }, inlineCode(e.detail)) : null),
          link(e));
      }));
    }
    const fmtWhen = (ms) => {
      const d = Date.now() - ms;
      if (d < 36 * 3600e3) return new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
      return fmtDay(ms);
    };
    function link(e) {
      const l = e.link || {};
      if (l.story) return h("button", { class: "lv-link hs-go", onclick: () => ctx.go(`#/story/${l.story.agent}`), title: "Open the agent's story at this moment" }, "Story");
      if (l.test) return h("button", { class: "lv-link hs-go", onclick: () => ctx.go("#/health/tests") }, "Test");
      if (l.incident) return h("button", { class: "lv-link hs-go", onclick: () => ctx.go(`#/health/${l.incident}`) }, "Board");
      if (l.question) return h("button", { class: "lv-link hs-go", onclick: () => ctx.go("#/decide") }, "Decide");
      if (l.error) return h("button", { class: "lv-link hs-go", onclick: () => ctx.go(`#/preview/${e.agent || ""}`) }, "Error");
      if (l.commit) return h("button", { class: "lv-link hs-go", onclick: () => ctx.go(`#/trace?q=${enc(e.kind === "revert" ? `why was ${l.commit} reverted` : `why did ${l.commit} change ${S.boards.get(S.sel)?.hotspot?.file || "this file"}`)}`) }, l.commit.slice(0, 7));
      return h("span");
    }

    function records(x) {
      const recs = x.records || [];
      const miss = S.data?.ctx && S.data.ctx.available === false ? S.data.ctx.why : null;
      if (!recs.length) return h("div", { class: "hs-none" }, miss ? h("span", { class: "lv-sub" }, miss) : [h("b", null, "Nothing names this file."), " No decision, fact or rule says how it should be: one reason agents keep changing it back and forth."]);
      return h("ul", { class: "hs-recs" }, recs.map((r) => h("li", null, ctxChip(r.via === "rule" ? "rule" : r.kind), h("span", null, r.text), r.ts ? h("span", { class: "lv-sub" }, fmtDay(r.ts)) : null)));
    }

    function told(x) {
      const text = x.context || contextOf(x);
      return h("div", { class: "hs-told" },
        h("pre", { class: "hs-ctx" }, text),
        h("div", { class: "lv-sub" }, "Once per session, the first time an agent's Edit or Write touches the file (its PreToolUse hook's additionalContext)."));
    }

    function actions(x) {
      const s = x.suggestion;
      const k = (a) => `${x.id}:${a}`;
      const tests = x.tests || [];
      const rule = S.ruleText.get(x.id) ?? (s && s.rule) ?? defaultRule(x);
      const ruleBox = h("textarea", { class: "hs-rule", rows: 2, "aria-label": "The rule", oninput: (e) => S.ruleText.set(x.id, e.target.value) });
      ruleBox.value = rule;
      const testSel = h("select", { class: "hs-sel", "aria-label": "Test", onchange: (e) => S.testPick.set(x.id, e.target.value) },
        tests.length ? tests.map((t) => h("option", { value: t.test, selected: (S.testPick.get(x.id) || x.pinned_test || s?.test) === t.test ? true : null }, `${t.test}${t.flaky ? " (flaky)" : t.failed ? ` (${t.failed} failed)` : ""}`)) : h("option", { value: "" }, "no test ran after edits here"));
      const models = ["opus", "sonnet"];
      const modelSel = h("select", { class: "hs-sel", "aria-label": "Model", onchange: (e) => S.modelPick.set(x.id, e.target.value) },
        models.map((m) => h("option", { value: m, selected: (S.modelPick.get(x.id) || x.routed_model || s?.model || "opus") === m ? true : null }, m)));
      const act = async (key, fn) => {
        S.busy.add(key); draw();
        try { S.done.set(key, await fn()); }
        catch (e) { S.done.set(key, { error: e.message || String(e) }); }
        S.busy.delete(key); draw();
      };
      const note = (key) => { const v = S.done.get(key); return v ? h("div", { class: v.error ? "err" : "hs-ok" }, v.error || v.ok) : null; };
      const rec = (kind) => s && s.kind === kind ? h("span", { class: "hs-rec" }, "suggested") : null;
      return h("div", { class: "hs-do" },
        s ? h("p", { class: "hs-sug" }, inlineCode(s.text)) : null,
        h("div", { class: "hs-a" + (s?.kind === "rule" ? " sug" : "") },
          h("div", { class: "hs-ah" }, h("b", null, "Add rule"), rec("rule"), h("span", { class: "lv-sub" }, "told to every agent of the repository, kept by ChloeCtx")),
          ruleBox,
          h("button", { class: "btn sm primary", disabled: S.busy.has(k("rule")) ? true : null, onclick: () => act(k("rule"), async () => {
            const text = (S.ruleText.get(x.id) ?? rule).trim();
            if (!text) throw new Error("Write the rule first.");
            const r = await ctx.api("/api/rules", { method: "POST", body: { repo: x.repo || S.data.repo, text } });
            cache.clear();
            return { ok: `Rule ${r.rule?.id || ""} added: every agent is told it from its next prompt.` };
          }) }, "Add rule"),
          note(k("rule"))),
        h("div", { class: "hs-a" + (s?.kind === "test" ? " sug" : "") },
          h("div", { class: "hs-ah" }, h("b", null, "Pin a test"), rec("test"), x.pinned_test ? pill(`pinned: ${x.pinned_test.split(/::| › /).pop()}`, "good") : h("span", { class: "lv-sub" }, "agents are told to run it before and after editing here")),
          h("div", { class: "hs-ar" }, testSel,
            h("button", { class: "btn sm", disabled: !tests.length || S.busy.has(k("pin")) ? true : null, onclick: () => act(k("pin"), async () => {
              const test = S.testPick.get(x.id) || testSel.value;
              const r = await ctx.api(`/api/hotspots/${enc(x.id)}/act`, { method: "POST", body: { kind: "pin_test", test } });
              Object.assign(x, r.hotspot || {}); S.boards.delete(x.id); fetchBoard(x.id).then(draw);
              return { ok: `Pinned ${test}.` };
            }) }, "Pin")),
          note(k("pin"))),
        h("div", { class: "hs-a" + (s?.kind === "model" ? " sug" : "") },
          h("div", { class: "hs-ah" }, h("b", null, "Route to a stronger model"), rec("model"), x.routed_model ? pill(`routed: ${x.routed_model}`, "acc") : h("span", { class: "lv-sub" }, "agents editing it are told the user wants this model for it")),
          h("div", { class: "hs-ar" }, modelSel,
            h("button", { class: "btn sm", disabled: S.busy.has(k("route")) ? true : null, onclick: () => act(k("route"), async () => {
              const model = S.modelPick.get(x.id) || modelSel.value;
              const r = await ctx.api(`/api/hotspots/${enc(x.id)}/act`, { method: "POST", body: { kind: "route", model } });
              Object.assign(x, r.hotspot || {}); S.boards.delete(x.id); fetchBoard(x.id).then(draw);
              return { ok: `Agents editing ${base(x.file)} are told you want ${model} for it.` };
            }) }, "Route")),
          note(k("route"))));
    }

    fetchList();
    sync();
    const poll = setInterval(() => { if (S.active) { S.boards.delete(S.sel); fetchList(); } }, 60000);
    return {
      update(state) { view.update(state); },
      unmount() { alive = false; mo.disconnect(); clearInterval(poll); syncSoon.cancel(); el.removeEventListener("click", onClick, true); document.removeEventListener("keydown", onKey, true); view.unmount(); },
    };
  };
}

/** A rule to start from: what you said about it, else its decision. */
function defaultRule(x) {
  const r = (x.records || [])[0];
  if (r) return r.text;
  return `Before changing ${base(x.file)}${x.symbol ? ` (${x.symbol})` : ""}, read why it is like this and run its tests; ask before reverting someone's change.`;
}
/** What an agent is told, when the server did not say it. */
function contextOf(x) {
  const lines = [`Workbench: ${x.file} is a hotspot (score ${fmtScore(x.score)}): agents keep correcting it.`];
  lines.push(`- ${x.summary}`);
  for (const r of (x.records || []).slice(0, 3)) lines.push(`- ◆ ${r.via === "rule" ? "rule" : r.kind}: ${r.text}`);
  if (x.suggestion) lines.push(x.suggestion.text.replace(/`/g, ""));
  return lines.join("\n");
}

// =========================================================== Mission badge
/** Wraps Mission's mount: a badge on the card of an agent on a hotspot. */
export function withHotBadges(mountMission) {
  return function mount(root, ctx) {
    const view = mountMission(root, ctx);
    const el = root.lastElementChild;
    let on = new Map(), alive = true, busy = false;
    async function fetchOn() {
      try {
        const repo = repoOf(ctx);
        const d = await ctx.api(`/api/hotspots/agents${repo ? `?repo=${enc(repo)}` : ""}`);
        on = new Map((d.agents || []).map((x) => [x.name, x]));
      } catch { on = new Map(); }
      apply();
    }
    function apply() {
      if (!alive || !el.isConnected) return;
      busy = true;
      try {
        for (const c of el.querySelectorAll(".mi-oc")) {
          const name = (c.querySelector(".mi-name")?.textContent || "").replace(/\s*·\s*$/, "").trim();
          const x = on.get(name);
          const had = c.querySelector(".hs-badge");
          if (!x) { had?.remove(); continue; }
          if (had) continue;
          c.querySelector(".mi-oc-h")?.append(h("button", {
            class: "hs-badge", style: { "--hs": heatColor(x.score) },
            title: `${name} is editing ${x.file}${x.symbol ? ` (${x.symbol})` : ""}, a hotspot (score ${fmtScore(x.score)}). It was told the history once. Click for the evidence.`,
            onclick: (e) => { e.stopPropagation(); ctx.go(`#/health/hotspots?h=${enc(x.hotspot)}`); },
          }, h("i", { class: "hs-sw" }), base(x.file)));
        }
      } finally { busy = false; }
    }
    const mo = new MutationObserver(() => { if (!busy) apply(); });
    mo.observe(el, { childList: true, subtree: true });
    fetchOn();
    const poll = setInterval(fetchOn, 20000);
    return {
      update(state) { view.update(state); },
      unmount() { alive = false; mo.disconnect(); clearInterval(poll); view.unmount(); },
    };
  };
}

useStyle("hotspots", `
.hs-sw{display:inline-block;width:12px;height:12px;border-radius:4px;background:linear-gradient(135deg,var(--warn),var(--crit));flex:none}
.hs-toggle{display:inline-flex;align-items:center;gap:7px;height:28px;padding:0 11px 0 9px;border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--text-2);cursor:pointer;font:600 12.5px var(--sans);margin-left:auto}
.hs-toggle:hover{border-color:var(--line-2);color:var(--text)}
.hs-toggle.on{border-color:color-mix(in srgb,var(--crit) 55%,var(--line));background:color-mix(in srgb,var(--crit) 12%,var(--surface));color:var(--text);box-shadow:0 0 0 2px color-mix(in srgb,var(--crit) 22%,transparent)}
.hs-tn{font-size:10.5px;min-width:17px;height:17px;padding:0 5px;border-radius:999px;display:inline-grid;place-items:center;background:var(--crit);color:#fff}
.mp-tm.hs-on .mp-c:not(.hs-h){background:var(--surface-2)!important;border-color:var(--line)!important;opacity:.55}
.mp-tm.hs-on .mp-c:not(.hs-h) .mp-cav,.mp-tm.hs-on .mp-c:not(.hs-h) .mp-cl{opacity:.6}
.mp-tm.hs-on .mp-f:not(.hs-h){background:var(--surface)!important;border-color:var(--line)!important}
.mp-tm.hs-on .mp-f.hs-h{background:var(--hs-a)!important;border-color:color-mix(in srgb,var(--hs) 55%,var(--line))!important}
.mp-tm.hs-on .mp-c.hs-h{background:var(--hs-a)!important;border:1.5px solid var(--hs)!important;opacity:1;box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--hs) 30%,transparent),0 0 18px color-mix(in srgb,var(--hs) 30%,transparent)}
.mp-tm.hs-on .mp-c.hs-h .mp-cn{color:var(--text);font-weight:650}
.mp-tm.hs-on .mp-c.hs-flash{outline:2px solid var(--text);outline-offset:1px}
.mp-tm.hs-on .mp-c.coll,.mp-tm.hs-on .mp-f.coll{background-image:none!important}
.hs-hb{position:absolute;right:4px;top:3px;font:700 10.5px var(--mono);color:#fff;background:var(--hs);border-radius:5px;padding:0 5px;line-height:16px}
.hs-tip{margin-top:8px;padding-top:7px;border-top:1px solid var(--line)}
.hs-tip-h{display:flex;align-items:center;gap:6px;font-weight:650;color:var(--hs)}
.hs-tip-t{margin-top:4px;color:var(--text)}
.hs-tip .hs-chips{margin:6px 0 4px}
.hs-mcard{border-color:color-mix(in srgb,var(--crit) 35%,var(--line))}
.hs-mh{display:flex;align-items:center;gap:6px;color:var(--crit-ink)}
.hs-ml{display:flex;flex-direction:column}
.hs-mr{display:flex;align-items:center;gap:10px;border:0;border-top:1px solid var(--line);background:none;padding:8px 2px;text-align:left;font:inherit;color:var(--text);cursor:pointer}
.hs-mr:first-child{border-top:0}.hs-mr:hover{background:var(--surface-2)}
.hs-mt{flex:1;min-width:0;display:flex;flex-direction:column;gap:4px}
.hs-mf{font:12px var(--mono);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.hs-dir{color:var(--muted)}.hs-sym{color:var(--text-2)}
.hs-all{margin-top:8px;font-size:12.5px}
.hs-miss{margin-top:6px}
.hs-score{display:inline-flex;align-items:baseline;gap:5px;flex:none;min-width:30px;justify-content:center;border-radius:7px;padding:2px 6px;background:color-mix(in srgb,var(--hs) 16%,transparent);color:var(--hs);font:700 12.5px var(--mono);box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--hs) 40%,transparent)}
.hs-score.big{flex-direction:column;align-items:center;gap:0;padding:6px 12px;border-radius:10px}
.hs-score.big b{font-size:24px;line-height:1.1}.hs-score.big span{font:650 10.5px var(--sans);text-transform:uppercase;letter-spacing:.06em}
.hs-chips{display:flex;flex-wrap:wrap;gap:4px;align-items:center}
.hs-rc{display:inline-flex;align-items:center;border-radius:999px;padding:1px 8px;font-size:11px;font-weight:600;white-space:nowrap}
.hs-rc.t-warn{background:var(--warn-soft);color:var(--warn-ink)}.hs-rc.t-crit{background:var(--crit-soft);color:var(--crit-ink)}
.hs-rc.t-acc{background:var(--accent-soft);color:var(--accent)}.hs-rc.t-mut{background:var(--surface-3);color:var(--text-2)}
.hs-spark{display:block;flex:none}
.hs-tab{display:inline-flex;align-items:center;gap:6px}
.hs-panel{display:flex;flex-direction:column;gap:16px}
.hs-stats{display:grid;grid-template-columns:repeat(6,minmax(0,1fr)) auto;gap:14px;align-items:center;padding:16px 20px}
.hs-tile{display:flex;flex-direction:column;gap:2px;min-width:0}
.hs-tile>b{font-size:30px;line-height:1.05;font-weight:720;letter-spacing:-.02em}
.hs-tile>b.t-crit{color:var(--crit-ink)}.hs-tile>b.t-warn{color:var(--warn-ink)}.hs-tile>b.t-acc{color:var(--accent)}
.hs-tl{font-weight:650}
.hs-win{display:flex;flex-direction:column;align-items:flex-end;gap:6px}
.hs-filters{display:flex;gap:2px;padding:2px;border-radius:999px;background:var(--sunken);border:1px solid var(--line)}
.hs-f{border:0;background:none;border-radius:999px;padding:2px 10px;font:600 11.5px var(--sans);color:var(--muted);cursor:pointer}
.hs-f:hover{color:var(--text)}.hs-f.on{background:var(--accent-soft);color:var(--accent)}
.hs-grid{display:grid;grid-template-columns:440px minmax(0,1fr);gap:18px;align-items:start}
.hs-list{display:flex;flex-direction:column;gap:8px;min-width:0}
.hs-lh{display:flex;align-items:center;justify-content:space-between;gap:8px}.hs-lh .lv-h{margin:0}
.hs-calm{padding:14px;border:1px dashed var(--line-2);border-radius:12px;color:var(--muted)}.hs-calm b{color:var(--good-ink)}
.hs-row{position:relative;display:flex;align-items:stretch;gap:10px;text-align:left;width:100%;border:1px solid var(--line);background:var(--surface);border-radius:12px;padding:11px 12px 11px 16px;cursor:pointer;font:inherit;color:inherit;box-shadow:var(--shadow-1);overflow:hidden}
.hs-row:hover{border-color:var(--line-2)}
.hs-row.on{border-color:var(--hs);box-shadow:0 0 0 1px var(--hs);background:color-mix(in srgb,var(--hs) 7%,var(--surface))}
.hs-heat{position:absolute;left:0;top:0;bottom:0;width:5px;background:var(--hs)}
.hs-rank{font:650 12px var(--mono);color:var(--muted);width:14px;padding-top:1px}
.hs-rb{flex:1;min-width:0;display:flex;flex-direction:column;gap:5px}
.hs-rf{font:12.5px var(--mono);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.hs-rf b{font-weight:700}
.hs-rs{display:flex;align-items:center;gap:6px;font:12px var(--mono);color:var(--text-2)}
.hs-kind{font:600 10px var(--sans);text-transform:uppercase;letter-spacing:.05em;color:var(--muted);border:1px solid var(--line-2);border-radius:4px;padding:0 4px}
.hs-rm{display:flex;align-items:center;gap:8px;color:var(--muted);font-size:11.5px}
.hs-avs{display:inline-flex;gap:3px}
.hs-rr{display:flex;flex-direction:column;align-items:flex-end;justify-content:space-between;gap:8px}
.hs-board{padding:18px 20px;display:flex;flex-direction:column;gap:14px;min-width:0;border-top:3px solid var(--hs)}
.hs-bh{display:flex;align-items:flex-start;justify-content:space-between;gap:14px}
.hs-bt{min-width:0}
.hs-bf{font:16px var(--mono);overflow-wrap:anywhere}.hs-bf b{font-weight:700}
.hs-sum{margin:6px 0 0;font-size:14px;line-height:1.5;color:var(--text)}
.hs-acts{display:flex;gap:8px;flex-wrap:wrap}
.hs-bgrid{display:grid;grid-template-columns:minmax(0,1.1fr) minmax(0,1fr);gap:20px}
.hs-col{display:flex;flex-direction:column;gap:16px;min-width:0}
.hs-sec .lv-h{margin-bottom:8px}
.hs-why{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:7px}
.hs-why li{display:grid;grid-template-columns:auto minmax(0,1fr) 84px;align-items:center;gap:9px}
.hs-wt{color:var(--text-2)}
.hs-pts{position:relative;display:flex;align-items:center;justify-content:flex-end;font:600 11px var(--mono);color:var(--muted);height:16px}
.hs-pts i{position:absolute;left:0;top:6px;height:4px;border-radius:2px;background:var(--hs);max-width:52px}
.hs-tl{list-style:none;margin:0;padding:0;display:flex;flex-direction:column}
.hs-ev{display:grid;grid-template-columns:62px 14px minmax(0,1fr) auto;gap:8px;align-items:start;padding:6px 0;position:relative}
.hs-ev::before{content:"";position:absolute;left:68px;top:0;bottom:0;width:2px;background:var(--line)}
.hs-ev:first-child::before{top:14px}.hs-ev:last-child::before{bottom:calc(100% - 14px)}
.hs-when{font:11px var(--mono);color:var(--muted);text-align:right;padding-top:2px}
.hs-dot{width:10px;height:10px;border-radius:50%;background:var(--c);margin-top:4px;position:relative;z-index:1;box-shadow:0 0 0 3px var(--surface)}
.hs-ev.k-record .hs-dot{border-radius:2px;transform:rotate(45deg) scale(.9)}
.hs-eb{min-width:0}
.hs-eh{display:flex;align-items:center;gap:8px}
.hs-ek{font-size:10.5px;font-weight:700;text-transform:uppercase;letter-spacing:.05em;color:var(--c)}
.hs-ea{display:inline-flex;align-items:center;gap:5px;color:var(--text-2);font-size:12px}
.hs-br{font:11px var(--mono);color:var(--muted);background:var(--surface-2);border-radius:5px;padding:0 5px}
.hs-et{margin-top:1px}
.hs-ed{font-size:12px;margin-top:1px}
.hs-go{font-size:12px;padding-top:2px}
.hs-churn{border:1px solid var(--line);border-radius:10px;padding:10px 12px 6px;background:var(--surface-2)}
.hs-churn .hs-spark{width:100%;height:40px}
.hs-axis{display:flex;justify-content:space-between;color:var(--muted);font-size:11px;margin-top:4px}
.hs-syms{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:6px}
.hs-syms li{display:grid;grid-template-columns:auto auto minmax(0,1fr) 22px;align-items:center;gap:6px}
.hs-syms code{font:12px var(--mono)}
.hs-sbar{height:6px;border-radius:999px;background:var(--surface-3);overflow:hidden}.hs-sbar b{display:block;height:100%;background:var(--hs);border-radius:999px}
.hs-sn{font:600 11.5px var(--mono);text-align:right;color:var(--text-2)}
.hs-recs{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:8px}
.hs-recs li{display:grid;grid-template-columns:auto minmax(0,1fr) auto;gap:8px;align-items:baseline}
.hs-none{padding:10px 12px;border:1px dashed color-mix(in srgb,var(--ctx) 40%,var(--line));border-radius:10px;color:var(--text-2)}
.hs-told{display:flex;flex-direction:column;gap:6px}
.hs-ctx{margin:0;font:11.5px/1.55 var(--mono);white-space:pre-wrap;overflow-wrap:anywhere;background:var(--sunken);border:1px solid var(--line);border-left:3px solid var(--ctx);border-radius:8px;padding:9px 11px;color:var(--text-2)}
.hs-do{display:flex;flex-direction:column;gap:10px}
.hs-sug{margin:0;padding:8px 11px;border-radius:9px;background:color-mix(in srgb,var(--hs) 10%,transparent);color:var(--text)}
.hs-a{border:1px solid var(--line);border-radius:10px;padding:10px 12px;display:flex;flex-direction:column;gap:8px}
.hs-a.sug{border-color:color-mix(in srgb,var(--accent) 55%,var(--line));box-shadow:0 0 0 1px color-mix(in srgb,var(--accent) 25%,transparent)}
.hs-ah{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
.hs-rec{font-size:10.5px;font-weight:700;text-transform:uppercase;letter-spacing:.05em;color:var(--accent)}
.hs-a .btn{align-self:flex-start}
.hs-ar{display:flex;gap:8px;align-items:center}
.hs-rule,.hs-sel{font:12.5px var(--sans);color:var(--text);background:var(--surface-2);border:1px solid var(--line-2);border-radius:8px;padding:6px 9px;min-width:0}
.hs-rule{resize:vertical;width:100%;box-sizing:border-box}
.hs-sel{flex:1}
.hs-ok{color:var(--good-ink);font-size:12.5px}
.hs-badge{display:inline-flex;align-items:center;gap:5px;margin-left:6px;border:0;border-radius:999px;padding:1px 8px 1px 6px;background:color-mix(in srgb,var(--hs) 16%,transparent);color:var(--hs);font:650 11px var(--mono);cursor:pointer;box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--hs) 40%,transparent)}
.hs-badge .hs-sw{width:9px;height:9px;border-radius:3px}
@media (max-width:1180px){.hs-grid{grid-template-columns:360px minmax(0,1fr)}.hs-bgrid{grid-template-columns:1fr}.hs-stats{grid-template-columns:repeat(3,minmax(0,1fr))}.hs-win{grid-column:1/-1;flex-direction:row;align-items:center}}
@media (max-width:860px){.hs-grid{grid-template-columns:1fr}}
`);
