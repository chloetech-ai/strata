// Logs: the server's latest 5,000 lines, followed live. Filter by level,
// text or request id (a request's id links its queries and its lines),
// see the rate of lines by level, and change what the server logs while
// it runs (debug for a while, then back).

import { h, s, icon, clear, fmt, card, api, every, toast, seg, debounce, failure, copy, showTip, hideTip, store, ApiError } from "../core.js";
import { state } from "../data.js";

export default { id: "logs", title: "Logs", icon: "logs", group: "Workload", keywords: "log lines tail errors warnings request id filter level debug", render, onParams };

const LEVELS = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];
const LCOLOR = { ERROR: "var(--crit)", WARN: "var(--s4)", INFO: "var(--s1)", DEBUG: "var(--muted)", TRACE: "var(--line-2)" };
const PRESETS = ["error", "warn", "info", "debug", "info,strata_server=debug", "info,strata=debug", "trace"];

let setRequest = null;
function onParams(r) { setRequest?.(r.params.get("request") || ""); }

function render(page, r) {
  const f = { level: store.get("logs.level", "info"), q: "", request: r.params.get("request") || "", follow: true };
  let lines = [], lastSeq = 0, filterNow = state.system?.logs?.filter || "";
  const box = h("div", { class: "logs", role: "log", "aria-live": "off" });
  const rate = h("div");
  const count = h("span", { class: "small muted" });
  const lvl = seg(["error", "warn", "info", "debug", "trace"].map((x) => [x, x]), f.level, (v) => { f.level = v; store.set("logs.level", v); reload(); }, { title: "The least severe level shown" });
  const q = h("input", { class: "input sm", placeholder: "Search the text…", style: { width: "220px" }, oninput: debounce(() => { f.q = q.value; reload(); }, 250) });
  const req = h("input", { class: "input sm mono", placeholder: "request id", value: f.request, style: { width: "190px" }, oninput: debounce(() => { f.request = req.value.trim(); reload(); }, 250) });
  const follow = h("button", { class: "chip", "aria-pressed": "true", title: "Scroll to each new line", onclick: () => { f.follow = !f.follow; follow.setAttribute("aria-pressed", String(f.follow)); if (f.follow) box.scrollTop = box.scrollHeight; } }, icon("down"), "follow");
  const filt = h("input", { class: "input sm mono", value: filterNow, style: { width: "230px" }, list: "log-presets", title: "A tracing filter: a level, or target=level pairs" });
  const presets = h("datalist", { id: "log-presets" }, PRESETS.map((p) => h("option", { value: p })));
  const apply = h("button", { class: "btn sm", onclick: async () => {
    try { const j = await api("/v1/logs/filter", { method: "PUT", body: { filter: filt.value.trim() } }); filterNow = j.filter; toast(`The server now logs: ${j.filter}`, "ok"); }
    catch (e) { toast(e instanceof ApiError && e.status === 403 ? "Changing the log level needs an admin key" : e.message, "bad"); }
  } }, icon("check"), "Apply");
  page.append(
    h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, "Logs"), h("p", {}, "The latest lines the server wrote, as they come. Click a line for its fields; click a request id to see only that request.")),
      h("div", { class: "tools" }, h("span", { class: "small muted" }, "The server logs"), filt, presets, apply)),
    h("div", { class: "row", style: { gap: "10px", marginBottom: "10px" } }, lvl, q, req, follow,
      h("button", { class: "btn sm ghost", title: "Download the lines shown, as JSON Lines", onclick: () => download() }, icon("download"), "Download"),
      h("span", { class: "sep" }), count),
    card({ title: "Lines per 10 seconds", sub: "of those loaded, by level", body: rate, attrs: { style: { marginBottom: "10px" } } }),
    box);

  function row(l) {
    const ln = h("div", { class: "ln" },
      h("span", { class: "muted", title: fmt.datetime(l.ts_ms) }, fmt.time(l.ts_ms) + "." + String(l.ts_ms % 1000).padStart(3, "0")),
      h("span", { class: "lv " + l.level }, l.level),
      h("span", { class: "tg", title: l.target }, l.route ? `${l.method || ""} ${l.route}` : l.target),
      h("span", { class: "msg" }, l.message,
        Object.entries(l.fields || {}).slice(0, 6).map(([k, v]) => h("span", { class: "fld" }, "  ", h("b", {}, k + "="), typeof v === "string" ? v : JSON.stringify(v))),
        l.request_id ? h("span", {}, "  ", h("span", { class: "rid", title: "Only this request", onclick: (e) => { e.stopPropagation(); req.value = l.request_id; f.request = l.request_id; reload(); } }, l.request_id)) : null));
    ln.onclick = () => {
      if (ln.classList.toggle("open")) ln.append(h("div", { class: "more" },
        h("pre", { class: "code", style: { fontSize: "11.5px" } }, JSON.stringify(l, null, 2)),
        h("div", { class: "row", style: { marginTop: "6px" } }, h("button", { class: "btn sm", onclick: (e) => { e.stopPropagation(); copy(JSON.stringify(l)); } }, icon("copy"), "Copy"),
          l.request_id ? h("a", { class: "btn sm", href: "#/queries?tab=recent", onclick: (e) => e.stopPropagation() }, "Its queries", icon("right")) : null)));
      else ln.querySelector(".more")?.remove();
    };
    return ln;
  }

  function drawRate() {
    if (!lines.length) { clear(rate, h("div", { class: "small muted" }, "No lines at this level yet.")); return; }
    const t1 = Date.now(), t0 = Math.min(lines[0].ts_ms, t1 - 600e3), B = 10e3;
    const n = Math.ceil((t1 - t0) / B);
    const bins = Array.from({ length: n }, () => ({}));
    for (const l of lines) { const i = Math.min(n - 1, Math.floor((l.ts_ms - t0) / B)); bins[i][l.level] = (bins[i][l.level] || 0) + 1; }
    const max = Math.max(1, ...bins.map((b) => Object.values(b).reduce((a, x) => a + x, 0)));
    const W = 1000, H = 64, bw = W / n;
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${H + 14}`, style: { width: "100%", height: H + 14 + "px" }, preserveAspectRatio: "none" });
    bins.forEach((b, i) => {
      let y = H;
      for (const L of LEVELS) {
        const v = b[L] || 0; if (!v) continue;
        const hh = v / max * (H - 4);
        y -= hh;
        const rc = s("rect", { x: i * bw + 0.5, y, width: Math.max(1, bw - 1), height: hh, style: { fill: LCOLOR[L] } });
        rc.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `${fmt.time(t0 + i * B)} – ${fmt.time(t0 + (i + 1) * B)}`), ...LEVELS.filter((x) => b[x]).map((x) => h("div", { class: "r" }, h("span", { class: "k box", style: { background: LCOLOR[x] } }), h("b", {}, fmt.n(b[x])), h("span", { class: "n" }, x.toLowerCase())))]));
        rc.addEventListener("pointerleave", hideTip);
        svgEl.append(rc);
      }
    });
    clear(rate, svgEl, h("div", { class: "legend", style: { marginTop: "4px" } }, LEVELS.map((L) => h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "k box", style: { background: LCOLOR[L], width: "10px", height: "10px", borderRadius: "3px", display: "inline-block" } }), L.toLowerCase()))));
  }

  function params(after) {
    const p = new URLSearchParams({ after: String(after), limit: "1000", level: f.level });
    if (f.q) p.set("q", f.q);
    if (f.request) p.set("request", f.request);
    return p;
  }
  async function poll() {
    try {
      const j = await api("/v1/logs?" + params(lastSeq));
      if (j.filter && document.activeElement !== filt) { filt.value = j.filter; filterNow = j.filter; }
      for (const l of j.lines) l.level = String(l.level).toUpperCase();
      if (j.lines.length) {
        const atEnd = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
        lines.push(...j.lines);
        lastSeq = j.lines[j.lines.length - 1].seq;
        box.querySelector(":scope > .empty")?.remove();
        box.append(...j.lines.map(row));
        if (lines.length > 3000) { const cut = lines.length - 3000; lines.splice(0, cut); for (let i = 0; i < cut; i++) box.firstChild?.remove(); }
        if (f.follow && (atEnd || j.lines.length === lines.length)) box.scrollTop = box.scrollHeight;
        drawRate();
      } else if (!lines.length) { clear(box, h("div", { class: "empty" }, icon("logs"), h("b", {}, "No lines match"), h("div", { class: "small" }, f.level === "info" || f.level === "warn" || f.level === "error" ? "Requests log at debug: set the server to debug above to see each one." : "Try a lower level or clear the filters."))); drawRate(); }
      count.textContent = `${fmt.plural(lines.length, "line")} · newest #${j.last}`;
    } catch (e) { clear(box, failure(e, "The logs")); stop(); }
  }
  function reload() { lines = []; lastSeq = 0; clear(box); poll(); }
  function download() {
    const blob = new Blob([lines.map((l) => JSON.stringify(l)).join("\n") + "\n"], { type: "application/x-ndjson" });
    const a = h("a", { href: URL.createObjectURL(blob), download: `strata-logs-${new Date().toISOString().slice(0, 19)}.jsonl` });
    document.body.append(a); a.click(); a.remove();
  }
  setRequest = (id) => { req.value = id; f.request = id; reload(); };
  const stop = every(1500, poll);
  return () => { stop(); setRequest = null; };
}
