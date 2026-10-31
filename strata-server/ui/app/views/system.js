// System: the process and its limits, the database file and its
// segments (a map of them, by table, sized by rows, deleted rows marked),
// what the merges do, the operations an admin runs (check, vacuum,
// checkpoint, verify replicas, a snapshot lease), the configuration and
// the raw Prometheus metrics.

import { h, s, icon, clear, fmt, card, api, every, toast, confirm, drawer, modal, pill, empty, failure, debounce, copy, showTip, hideTip, bus, ApiError } from "../core.js";
import { SERIES, gauge } from "../charts.js";
import { state, refreshSystem, last } from "../data.js";

export default { id: "system", title: "System", icon: "system", group: "Admin", keywords: "process limits segments merges operations vacuum check checkpoint verify snapshot config metrics prometheus disk", render };

const kv = (pairs) => h("dl", { class: "kv" }, pairs.filter(Boolean).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v ?? "–")]));

function render(page) {
  const proc = h("div"), db = h("div"), limits = h("div"), ops = h("div"), segs = h("div"), merges = h("div"), cfg = h("div"), prom = h("div");
  page.append(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "System"), h("p", {}, "The process, its limits, the database on disk, and the operations to run on it."))),
    h("div", { class: "grid g3", style: { marginBottom: "14px" } }, card({ title: "Process", body: proc }), card({ title: "Database", body: db }), card({ title: "Limits", body: limits })),
    card({ title: "Operations", sub: "admin scope", body: ops, attrs: { style: { marginBottom: "14px" } } }),
    h("div", { class: "grid g3", style: { marginBottom: "14px" } },
      h("div", { class: "span2", style: { display: "contents" } }, card({ title: "Segments", sub: "each box is a segment, its width its rows; red is deleted rows", body: segs, cls: "span2" })),
      card({ title: "Merges", body: merges })),
    h("div", { class: "grid g2" }, card({ title: "Configuration", sub: "GET /v1/config", body: cfg }), card({ title: "Prometheus metrics", sub: "GET /metrics", body: prom })));

  function drawSys() {
    const y = state.system;
    if (!y) { clear(proc, empty("system", "Loading…")); return; }
    const p = last() || {};
    clear(proc, kv([["version", y.version], ["backend", `${y.backend} · ${y.role}`], ["up", `${fmt.dur(y.uptime_s)} (since ${fmt.datetime(y.started_ms)})`], ["pid", y.pid], ["platform", `${y.os} ${y.arch}`], ["SIMD", y.simd], ["cores", y.cores], ["memory", `${fmt.bytes(y.rss_bytes)} resident${y.memory_limit_bytes ? " of " + fmt.bytes(y.memory_limit_bytes) : ""}`], ["CPU time", fmt.dur(y.cpu_seconds)], ["log filter", h("span", { class: "mono" }, y.logs?.filter)], ["sampling", `a point every ${fmt.ms(y.sampling.interval_ms)}, ${fmt.n(y.sampling.points)} kept`]]));
    if (y.db) {
      const d = y.db.disk;
      clear(db, h("div", { class: "row nw", style: { gap: "16px", alignItems: "center" } },
        d ? gauge(1 - d.available_bytes / d.total_bytes, { size: 110, label: "disk used" }) : null,
        h("div", { class: "grow" }, kv([["path", h("span", { class: "mono small" }, y.db.path)], ["tables", y.db.tables], ["sequence", y.db.seq_no ? `term ${y.db.seq_no.term} · ${fmt.n(y.db.seq_no.seq)}` : "not numbered"], d ? ["disk", `${fmt.bytes(d.available_bytes)} free of ${fmt.bytes(d.total_bytes)}`] : null, ["rows", fmt.n(p.docs)], ["file", fmt.bytes(p.file_bytes)], ["log", fmt.bytes(p.wal_bytes)]]))));
    } else clear(db, h("div", { class: "small muted" }, "A coordinator holds no database; its shards do (Topology)."));
    const L = y.limits, F = y.features;
    clear(limits, kv([["timeout", `${fmt.ms(L.timeout_ms)} (max ${fmt.ms(L.max_timeout_ms)})`], ["concurrent queries", L.max_concurrent], ["rows per answer", fmt.n(L.max_rows)], ["groups", fmt.n(L.max_groups)], ["query memory", L.query_memory_bytes ? fmt.bytes(L.query_memory_bytes) : "no budget"], ["queued writes", fmt.n(L.max_queued_writes)], ["body", fmt.bytes(L.max_body_bytes)], ["slow queries", L.slow_ms ? `over ${L.slow_ms} ms` : "not kept"], ["leases", `${L.lease_ttl_s} s`], ["synonym rules", F.synonyms], ["models", F.models], ["backups", F.backup_dir ? h("span", { class: "mono small" }, F.backup_dir) : "no --backup-dir"], ["agent key", F.agent_key ? "set" : "not set"], ["token", y.auth?.token ? "set" : "none"]]));
    drawOps();
  }

  function drawOps() {
    const y = state.system;
    const local = y?.backend === "local";
    const primary = state.health?.replication?.role === "primary";
    const op = (ic, title, sub, run, { danger, disabled, why } = {}) => h("div", { class: "advice", style: { opacity: disabled ? 0.55 : 1 } },
      h("div", { class: "ic" }, icon(ic)), h("div", { class: "grow" }, h("h4", {}, title), h("p", {}, sub), disabled && why ? h("p", { class: "small muted", style: { marginTop: "4px" } }, why) : null),
      h("button", { class: "btn sm" + (danger ? " danger" : ""), disabled, onclick: run }, "Run"));
    clear(ops, h("div", { class: "grid g3" },
      op("okc", "Check the database", "Reads every page and checks every checksum and structure. Takes a while on a large file.", () => run("Check", "/v1/admin/check", {}, "Reads the whole file; queries keep running."), { disabled: !local, why: "On a server with a database." }),
      op("disk", "Vacuum", "Returns free space at the end of the file to the file system: moves segments down and truncates the tail.", () => run("Vacuum", "/v1/admin/vacuum", {}, "Rewrites the end of the file; writes wait meanwhile.", true), { disabled: !local, why: "On a server with a database." }),
      op("download", "Write a checkpoint", "A consistent copy of the database into --backup-dir, as one file or a directory.", () => checkpoint(), { disabled: !local || !y?.features?.backup_dir, why: local ? "Start the server with --backup-dir." : "On a server with a database." }),
      op("topology", "Verify the replicas", "Compares every replica's rows with this primary's; a replica that differs is sent a copy.", () => run("Verify", "/v1/admin/verify", { timeout_ms: 60000 }, "Asks each replica for a digest of its rows."), { disabled: !primary, why: "On a primary with replicas." }),
      op("clock", "Take a snapshot lease", "Pins the current version so that several queries read the same rows (pass its id as \"snapshot\").", () => run("Snapshot", "/v1/snapshot", {}, null), { disabled: !local }),
      op("refresh", "Reset statement statistics", "Starts the per-statement counts of Queries again.", () => run("Reset statistics", "/v1/queries/stats/reset", {}, "Every count starts again from zero.", true))));
  }

  async function run(what, path, body, warn, danger) {
    if (warn && !await confirm({ title: `${what}?`, body: warn, ok: what, danger })) return;
    const t0 = performance.now();
    toast(`${what}: running…`);
    try {
      const j = await api(path, { body });
      const ms = performance.now() - t0;
      drawer({ title: `${what}: done`, sub: `in ${fmt.ms(ms)}`, body: [pill("ok", "succeeded"), h("pre", { class: "code" }, JSON.stringify(j, null, 2))] });
      loadStats(); refreshSystem();
    } catch (e) { toast(`${what}: ${e instanceof ApiError && e.status === 403 ? "needs an admin key" : e.message}`, "bad"); }
  }

  function checkpoint() {
    const name = "cc-" + new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-");
    const inp = h("input", { class: "input", value: name });
    const layout = h("select", { class: "select" }, h("option", { value: "file" }, "one file"), h("option", { value: "dir" }, "a directory"));
    const go = h("button", { class: "btn primary" }, "Write it");
    const m = modal({ title: "Write a checkpoint", body: [h("label", { class: "f" }, `Name, in ${state.system.features.backup_dir}`, inp), h("label", { class: "f" }, "Layout", layout)], foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go] });
    go.onclick = () => { m.close(); run("Checkpoint", "/v1/admin/checkpoint", { name: inp.value.trim(), layout: layout.value }, null); };
  }

  async function loadStats() {
    try {
      const j = await api("/v1/stats");
      const st = j.storage, mg = j.merges;
      if (!st) { clear(segs, h("div", { class: "small muted" }, "A coordinator holds no segments.")); clear(merges, h("div", { class: "small muted" }, "–")); return; }
      const byTable = {};
      for (const sg of st.segments) (byTable[sg.table] ||= []).push(sg);
      const maxDocs = Math.max(1, ...st.segments.map((x) => x.n_docs));
      const tables = Object.entries(byTable);
      clear(segs, tables.length ? tables.map(([t, list], ti) => {
        const total = list.reduce((a, x) => a + x.n_docs, 0);
        const W = 1000, H = 34;
        const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, style: { width: "100%", height: H + "px" }, preserveAspectRatio: "none" });
        let x = 0;
        for (const sg of list.slice().sort((a, b) => b.n_docs - a.n_docs)) {
          const w = Math.max(3, sg.n_docs / total * W);
          const dead = sg.n_deleted / Math.max(1, sg.n_docs);
          const g = s("g");
          g.append(s("rect", { x: x + 1, y: 2, width: Math.max(1, w - 2), height: H - 4, rx: 4, style: { fill: SERIES[ti % 8], opacity: 0.35 + 0.65 * Math.sqrt(sg.n_docs / maxDocs) } }));
          if (dead > 0) g.append(s("rect", { x: x + 1, y: H - 8, width: Math.max(1, (w - 2) * dead), height: 6, rx: 2, style: { fill: "var(--crit)" } }));
          g.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `${t} · segment ${sg.id}`), h("div", { class: "r" }, h("b", {}, fmt.n(sg.n_docs) + " rows"), h("span", { class: "n" }, fmt.bytes(sg.bytes))), h("div", { class: "r" }, h("b", {}, fmt.n(sg.n_deleted)), h("span", { class: "n" }, "deleted, " + fmt.pct(dead)))]));
          g.addEventListener("pointerleave", hideTip);
          svgEl.append(g);
          x += w;
        }
        return h("div", { style: { marginBottom: "12px" } }, h("div", { class: "row nw between small", style: { marginBottom: "4px" } }, h("b", {}, t), h("span", { class: "muted" }, `${fmt.plural(list.length, "segment")} · ${fmt.n(total)} rows · ${fmt.bytes(list.reduce((a, x2) => a + x2.bytes, 0))}`)), svgEl);
      }) : empty("layers", "No segments yet", "Rows wait in the memtable until it flushes."),
      h("div", { class: "row small muted", style: { gap: "16px", marginTop: "6px" } }, `memtable: ${fmt.n(st.memtable_docs)} rows`, `pending deletes: ${fmt.n(st.pending_deletes)}`, `pages: ${fmt.n(st.live_pages)} live · ${fmt.n(st.free_pages)} free · ${fmt.n(st.pending_pages)} held`));
      clear(merges, mg ? kv([["running", `${mg.running} (${mg.segments_merging} segments)`], ["memory", `${fmt.bytes(mg.memory)}${mg.memory_budget ? " of " + fmt.bytes(mg.memory_budget) : ""}`], ["done", fmt.n(mg.merges)], ["made smaller", fmt.n(mg.merges_limited)], ...Object.entries(mg).filter(([k]) => !["running", "segments_merging", "memory", "memory_budget", "merges", "merges_limited"].includes(k)).map(([k, v]) => [k.replace(/_/g, " "), typeof v === "object" ? JSON.stringify(v) : String(v)])]) : h("div", { class: "small muted" }, "–"));
    } catch (e) { clear(segs, failure(e, "Storage")); }
  }

  async function loadCfg() {
    try { const j = await api("/v1/config"); clear(cfg, h("pre", { class: "code", style: { maxHeight: "360px" } }, JSON.stringify(j, null, 2))); } catch (e) { clear(cfg, failure(e, "The configuration")); }
  }

  async function loadProm() {
    try {
      const text = await api("/metrics", { text: true });
      const q = h("input", { class: "input sm", placeholder: "Filter metrics…", style: { width: "100%", marginBottom: "8px" } });
      const pre = h("pre", { class: "code", style: { maxHeight: "320px" } });
      const draw = () => { const f = q.value.toLowerCase(); pre.textContent = text.split("\n").filter((l) => !f || l.toLowerCase().includes(f)).join("\n"); };
      q.oninput = debounce(draw, 120);
      draw();
      clear(prom, h("div", { class: "row nw", style: { gap: "6px" } }, q, h("button", { class: "btn sm", style: { marginBottom: "8px" }, onclick: () => copy(text, "Metrics copied") }, icon("copy"))), pre, h("div", { class: "small muted" }, `${fmt.n(text.split("\n").filter((l) => l && !l.startsWith("#")).length)} samples · scrape it with Prometheus at ${location.origin}/metrics`));
    } catch (e) { clear(prom, failure(e, "The metrics")); }
  }

  drawSys(); refreshSystem(); loadStats(); loadCfg(); loadProm();
  const offs = [bus.on("system", drawSys), bus.on("health", drawOps), every(5000, loadStats, false)];
  return () => offs.forEach((f) => f());
}
