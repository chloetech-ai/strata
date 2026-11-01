// Fleet: the whole cluster at once. A tile per shard, coloured by one
// measure at a time (its slowest copy's p99, searches a second, CPU, disk,
// how far its replicas are behind, merges, rows), with a dot per copy; the
// machines the copies run on beside it, linked to the tiles by hover. Fail
// machines here, as a what-if, to see which shards keep serving, which
// fail over and which stop taking writes. The advisor reads the same
// numbers and says what to change, with the change one click away. Every
// number is the copies' own, from the coordinator's `/v1/cluster`.

import { h, icon, clear, fmt, card, api, toast, confirm, pill, empty, bus, seg, store, tipOn } from "../core.js";
import { sparkline } from "../charts.js";
import { state, isCoordinator, refreshCluster } from "../data.js";

export default { id: "fleet", title: "Fleet", icon: "box", group: "Monitor", keywords: "fleet map heatmap shards copies replicas machines hosts nodes blast radius failure what if advisor p99 lag disk cpu merges", render };

const SLOTS = 4096;
const short = (url) => (url || "").replace(/^https?:\/\//, "");
const hostOf = (url) => { try { return new URL(url).hostname; } catch { return short(url).split(":")[0]; } };

// ---------------------------------------------------------------- lenses
// A measure of a copy and of a shard, and the bins of its colour ramp:
// `bins` are the upper bounds of the first six colours, the seventh is
// anything above. `rel` lenses are binned against the fleet's own spread.
const RAMP = ["var(--seq-1)", "var(--seq-2)", "var(--seq-3)", "var(--seq-4)", "var(--seq-5)", "var(--seq-6)", "var(--seq-7)"];
const LENSES = {
  p99: { label: "p99", title: "The 99th percentile of the searches each copy answered in its last two seconds", copy: (c) => c.load?.p99_ms, shard: "max", fmt: fmt.ms, bins: [2, 5, 10, 20, 50, 100], low: "fast", high: "slow" },
  qps: { label: "Searches/s", title: "Searches each copy answers a second", copy: (c) => c.load?.qps, shard: "sum", fmt: fmt.rate, rel: true, low: "idle", high: "busiest" },
  cpu: { label: "CPU", title: "The CPU each copy's process uses, as a share of its cores", copy: (c) => c.load?.cpu != null ? c.load.cpu / Math.max(1, c.cores || 1) : null, shard: "max", fmt: (v) => fmt.pct(v, 0), bins: [0.1, 0.25, 0.4, 0.55, 0.7, 0.85], low: "0%", high: "100%" },
  disk: { label: "Disk", title: "How full the volume each copy's database is on is", copy: (c) => c.load?.disk_used_ratio, shard: "max", fmt: (v) => fmt.pct(v, 0), bins: [0.3, 0.5, 0.6, 0.7, 0.8, 0.9], low: "empty", high: "full" },
  lag: { label: "Lag", title: "Transactions a replica has still to apply from its primary", copy: (c) => c.behind, shard: "max", fmt: (v) => fmt.si(v), bins: [0, 1, 10, 100, 1000, 10000], low: "caught up", high: "behind" },
  merges: { label: "Merges", title: "Merges running on each copy, and the segments its table is in", copy: (c) => c.load?.merges_running, shard: "max", fmt: (v) => fmt.n(v), bins: [0, 1, 2, 3, 4, 6], low: "none", high: "many" },
  rows: { label: "Rows", title: "Rows each shard holds, against the fleet's average: an uneven split shows here", copy: (c) => c.docs, shard: "primary", fmt: (v) => fmt.si(v), rel: true, low: "fewest", high: "most" },
};

/** The fleet as the views below need it, from `/v1/cluster`. */
function model(c) {
  const shards = (c?.shards || []).map((sh) => {
    const primary = sh.replicas.find((r) => r.takes_writes) || null;
    const reps = primary?.detail?.health?.replication?.replicas || [];
    const copies = sh.replicas.map((r) => {
      const f = r === primary ? null : reps.find((x) => x.url === r.url);
      return {
        url: r.url, host: hostOf(r.url), shard: sh.shard, primary: r === primary, down: r.status === "down", error: r.error, status: r.status,
        load: r.detail?.load || null, cores: r.detail?.system?.cores || null, docs: r.docs, segments: r.segments, rtt_ms: r.rtt_ms,
        behind: r === primary ? 0 : f ? (f.in_sync ? 0 : f.behind ?? null) : null, in_sync: r === primary ? true : f ? !!f.in_sync : null, copying: !!f?.copying,
        term: r.term, role: r.role, avoided: !!r.avoided,
      };
    });
    return { i: sh.shard, url: sh.url, status: sh.status, slots: sh.slots_held ?? 0, copies, primary: copies.find((x) => x.primary) || null };
  });
  const hosts = new Map();
  for (const sh of shards) for (const cp of sh.copies) { if (!hosts.has(cp.host)) hosts.set(cp.host, []); hosts.get(cp.host).push(cp); }
  return { shards, hosts: [...hosts.entries()].map(([host, copies]) => ({ host, copies })).sort((a, b) => a.host.localeCompare(b.host, undefined, { numeric: true })) };
}

function shardValue(lens, sh) {
  const L = LENSES[lens];
  const vals = sh.copies.filter((c) => !c.down).map(L.copy).filter((v) => v != null && !Number.isNaN(v));
  if (L.shard === "primary") return sh.primary && !sh.primary.down ? L.copy(sh.primary) : vals.length ? Math.max(...vals) : null;
  if (!vals.length) return null;
  return L.shard === "sum" ? vals.reduce((a, b) => a + b, 0) : Math.max(...vals);
}

/** A colour for value `v` of `lens`; `all` are the fleet's values, for relative lenses. */
function binner(lens, all) {
  const L = LENSES[lens];
  if (L.rel) {
    const xs = all.filter((v) => v != null);
    const lo = xs.length ? Math.min(...xs) : 0, hi = xs.length ? Math.max(...xs) : 1;
    return (v) => (v == null ? null : hi <= lo ? 3 : Math.min(6, Math.floor(((v - lo) / (hi - lo)) * 6.999)));
  }
  return (v) => { if (v == null) return null; const i = L.bins.findIndex((b) => v <= b); return i < 0 ? 6 : i; };
}

// ---------------------------------------------------------------- what if
/** What failing the machines in `failed` does to each shard. */
function blast(m, failed) {
  const out = { serving: [], failover: [], noWrites: [], dark: [] };
  for (const sh of m.shards) {
    const n = sh.copies.length;
    const hit = sh.copies.filter((c) => failed.has(c.host));
    if (!hit.length) continue;
    const up = sh.copies.filter((c) => !c.down && !failed.has(c.host)).length;
    const primaryHit = sh.primary && failed.has(sh.primary.host);
    if (up === 0) out.dark.push(sh);
    else if (up <= n / 2) out.noWrites.push(sh);
    else if (primaryHit) out.failover.push(sh);
    else out.serving.push(sh);
  }
  return out;
}

// ---------------------------------------------------------------- advice
/** What the numbers suggest changing, the worst first. */
function advise(m, c, managed) {
  const out = [];
  const add = (sev, title, why, shards, action) => out.push({ sev, title, why, shards, action });
  for (const sh of m.shards) {
    for (const cp of sh.copies.filter((x) => x.down)) add("crit", `A copy of shard ${sh.i} does not answer`, `${short(cp.url)}: ${cp.error || "no answer"}. ${sh.copies.length - 1 > sh.copies.length / 2 ? "The others keep the shard serving." : "Too few copies are left to take writes."}`, [sh.i], { label: "Open the Cluster view", go: "#/cluster" });
    if (sh.copies.length > 1 && new Set(sh.copies.map((x) => x.host)).size === 1) add("warn", `Every copy of shard ${sh.i} runs on ${sh.copies[0].host}`, "One machine failing takes the whole shard with it: run its copies on different machines.", [sh.i], null);
    if (sh.copies.length === 1) add("warn", `Shard ${sh.i} has a single copy`, "Losing its server loses the shard until it is back. A second and third copy keep it serving and taking writes through a failure.", [sh.i], managed ? { label: "Copies per shard", go: "#/cluster" } : { label: "Add a copy", go: "#/cluster" });
    for (const cp of sh.copies.filter((x) => !x.primary && !x.down)) {
      if (cp.copying) add("info", `A copy of shard ${sh.i} is being sent the primary's rows`, `${short(cp.url)} serves once it has them all and caught up.`, [sh.i], null);
      else if (cp.behind != null && cp.behind > 1000) add("warn", `A replica of shard ${sh.i} is ${fmt.si(cp.behind)} transactions behind`, `${short(cp.url)} applies its primary's writes more slowly than they come: its reads are older, and writes waiting for every copy wait for it.`, [sh.i], null);
    }
    const p = sh.copies.filter((x) => !x.down && x.load?.p99_ms != null && (x.load?.qps || 0) > 1);
    if (p.length > 1) {
      const sorted = p.map((x) => x.load.p99_ms).sort((a, b) => a - b);
      const worst = p.reduce((a, b) => (b.load.p99_ms > a.load.p99_ms ? b : a));
      if (worst.load.p99_ms > 3 * sorted[0] && worst.load.p99_ms > 20) add("warn", `${short(worst.url)} is the slow copy of shard ${sh.i}`, `Its p99 is ${fmt.ms(worst.load.p99_ms)} against ${fmt.ms(sorted[0])} on the fastest copy. A search waits for the slowest shard it asks, so this copy sets the tail.`, [sh.i], { label: "Profile it", go: "#/profile?tab=cpu&node=" + encodeURIComponent(worst.url) });
    }
  }
  // Full disks, a line a machine: its copies share it.
  for (const mh of m.hosts) {
    const full = mh.copies.filter((cp) => (cp.load?.disk_used_ratio ?? 0) > 0.8);
    if (!full.length) continue;
    const d = Math.max(...full.map((cp) => cp.load.disk_used_ratio));
    const free = Math.min(...full.map((cp) => cp.load.disk_free_bytes ?? Infinity));
    add(d > 0.9 ? "crit" : "warn", `${fmt.pct(d, 0)} of the disk on ${mh.host} is used`, `${Number.isFinite(free) ? fmt.bytes(free) + " left under " : "Under "}${fmt.plural(full.length, "copy", "copies")} (shard ${[...new Set(full.map((cp) => cp.shard))].join(", ")}). A database that cannot write refuses writes until it restarts: grow the volume, or add a shard and even out the slots so each holds less.`, [...new Set(full.map((cp) => cp.shard))], null);
  }
  const counts = m.shards.map((s) => s.slots);
  if (counts.length > 1) {
    const avg = SLOTS / counts.length, hi = Math.max(...counts), lo = Math.min(...counts);
    if (hi - lo > Math.max(2, avg * 0.1)) {
      const big = m.shards.filter((s) => s.slots === hi).map((s) => s.i);
      add("info", `The slots are spread unevenly: ${lo} to ${hi} a shard`, "A shard holding more slots holds more rows and answers more of each search. Moving slots evens them out while the cluster serves.", big, managed ? null : { label: "Even out the slots", run: async () => {
        if (!(await confirm({ title: "Even out the slots", body: "Slots move from the shards holding the most to those holding the fewest, a run at a time, while the cluster serves.", ok: "Move them" }))) return;
        try { await api("/v1/admin/rebalance", { body: {} }); toast("Evening out the slots: follow it in the Cluster view", "ok"); refreshCluster(); } catch (e) { toast("Even out the slots: " + e.message, "bad", 7000); }
      } });
    }
  }
  if (c?.changes && !c.changes.ok) add("info", "The cluster map cannot change now", c.changes.why || "", [], null);
  const rank = { crit: 0, warn: 1, info: 2 };
  return out.sort((a, b) => rank[a.sev] - rank[b.sev]);
}

// ---------------------------------------------------------------- the view
function render(page) {
  const head = h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Fleet"), h("p", {}, "Every shard and copy at once, coloured by one measure: hover a shard to find its copies, fail machines to see what keeps serving.")));
  const kpiBox = h("div"), barBox = h("div", { class: "row", style: { gap: "12px", marginBottom: "12px" } }), mainBox = h("div", { class: "fleet" });
  page.append(head, kpiBox, barBox, mainBox);

  let lens = store.get("fleet.lens", "p99");
  if (!LENSES[lens]) lens = "p99";
  let hover = null; // {shard} or {host}
  let sel = null; // a shard's number
  const failed = new Set();
  // What each shard measured, for the sparklines and the change of a minute.
  const history = new Map(); // lens -> shard -> [[t, v]]
  let kube = null;
  // Hover only lights things up: the elements stay, their classes change.
  let tileEls = new Map(), hostEls = new Map(), current = null;
  function applyHover() {
    const m = current;
    if (!m) return;
    const hosts = hover?.host ? new Set([hover.host]) : hover?.shard != null ? new Set(m.shards.find((x) => x.i === hover.shard)?.copies.map((x) => x.host) || []) : null;
    const shards = hover?.shard != null ? new Set([hover.shard]) : hover?.host ? new Set(m.hosts.find((x) => x.host === hover.host)?.copies.map((x) => x.shard) || []) : null;
    for (const [i, el] of tileEls) { el.classList.toggle("lit", !!shards?.has(i)); el.classList.toggle("faded", !!shards && !shards.has(i)); }
    for (const [k, el] of hostEls) { el.classList.toggle("lit", !!hosts?.has(k)); el.classList.toggle("faded", !!hosts && !hosts.has(k)); }
  }
  const hoverOn = (x) => { if (JSON.stringify(x) === JSON.stringify(hover)) return; hover = x; applyHover(); };

  const remember = (m) => {
    const t = Date.now();
    for (const k of Object.keys(LENSES)) {
      if (!history.has(k)) history.set(k, new Map());
      const hk = history.get(k);
      for (const sh of m.shards) {
        if (!hk.has(sh.i)) hk.set(sh.i, []);
        const a = hk.get(sh.i);
        a.push([t, shardValue(k, sh)]);
        while (a.length && a[0][0] < t - 15 * 60e3) a.shift();
      }
    }
  };
  const minuteAgo = (k, i) => {
    const a = history.get(k)?.get(i) || [];
    const t = Date.now() - 60e3;
    for (let j = a.length - 1; j >= 0; j--) if (a[j][0] <= t) return a[j][1];
    return null;
  };

  const lensSeg = seg(Object.entries(LENSES).map(([k, L]) => [k, L.label, L.title]), lens, (v) => { lens = v; store.set("fleet.lens", v); draw(); }, { "aria-label": "Colour the fleet by" });

  function legend() {
    const L = LENSES[lens];
    const cells = RAMP.map((col, i) => h("span", { class: "fleet-ramp", style: { background: col }, title: L.rel ? "" : i < 6 ? "up to " + L.fmt(L.bins[i]) : "over " + L.fmt(L.bins[5]) }));
    return h("div", { class: "row nw small muted", style: { gap: "6px" } }, h("span", {}, L.low), h("span", { class: "row nw", style: { gap: "2px" } }, cells), h("span", {}, L.high));
  }

  function draw() {
    const c = state.cluster;
    if (!isCoordinator()) {
      clear(kpiBox); clear(barBox);
      clear(mainBox, card({ body: empty("box", "This server is not a coordinator", "The fleet map shows a cluster's shards and copies, from the command center of a coordinator over them (strata-server --shards …).", h("a", { class: "btn sm", href: "#/cluster" }, icon("layers"), "How to make a cluster")) }));
      return;
    }
    if (!c) { clear(mainBox, empty("box", "Asking the shards", "")); return; }
    const m = current = model(c);
    tileEls = new Map(); hostEls = new Map();
    const managed = !!kube?.managed;
    const vals = m.shards.map((sh) => shardValue(lens, sh));
    const bin = binner(lens, vals);
    const copyVals = m.shards.flatMap((sh) => sh.copies.map(LENSES[lens].copy));
    const copyBin = LENSES[lens].rel ? binner(lens, copyVals) : bin;
    const L = LENSES[lens];

    // ---- the numbers on top
    const copies = m.shards.reduce((n, s) => n + s.copies.length, 0);
    const down = m.shards.flatMap((s) => s.copies.filter((x) => x.down)).length;
    const p99s = m.shards.map((sh) => [sh, shardValue("p99", sh)]).filter(([, v]) => v != null);
    const slow = p99s.length ? p99s.reduce((a, b) => (b[1] > a[1] ? b : a)) : null;
    const lags = m.shards.flatMap((sh) => sh.copies.filter((x) => !x.primary && x.behind != null).map((x) => [sh, x.behind]));
    const lag = lags.length ? lags.reduce((a, b) => (b[1] > a[1] ? b : a)) : null;
    const disks = m.shards.flatMap((sh) => sh.copies.filter((x) => x.load?.disk_used_ratio != null).map((x) => [sh, x.load.disk_used_ratio]));
    const disk = disks.length ? disks.reduce((a, b) => (b[1] > a[1] ? b : a)) : null;
    const kpi = (lbl, val, sub, lv, onclick) => card({ cls: onclick ? "hover" : "", attrs: onclick ? { style: { cursor: "pointer" }, onclick } : {}, body: h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "lbl" }, lbl), h("div", { class: "val", style: lv ? { color: lv } : {} }, val), sub ? h("div", { class: "delta", style: { maxWidth: "100%" } }, sub) : null) });
    clear(kpiBox, h("div", { class: "grid g4", style: { marginBottom: "12px" } },
      kpi("The fleet", `${fmt.n(m.shards.length)} × ${copies && m.shards.length ? fmt.n(Math.round((copies / m.shards.length) * 10) / 10) : 0}`, `${fmt.plural(m.shards.length, "shard")}, ${fmt.plural(copies, "copy", "copies")} on ${fmt.plural(m.hosts.length, "machine")}${down ? `, ${down} down` : ""}`, down ? "var(--crit-ink)" : null),
      kpi("Slowest shard", slow ? fmt.ms(slow[1]) : "–", slow ? `shard ${slow[0].i}'s p99, which every search waits for` : "no searches in the last seconds", slow && slow[1] > 100 ? "var(--crit-ink)" : null, slow ? () => { sel = slow[0].i; lens = "p99"; lensSeg.set("p99"); draw(); } : null),
      kpi("Furthest behind", lag ? fmt.si(lag[1]) : "–", lag ? (lag[1] ? `transactions, a replica of shard ${lag[0].i}` : "every replica is caught up") : "no replicas", lag && lag[1] > 1000 ? "var(--crit-ink)" : null, lag && lag[1] ? () => { sel = lag[0].i; lens = "lag"; lensSeg.set("lag"); draw(); } : null),
      kpi("Fullest disk", disk ? fmt.pct(disk[1], 0) : "–", disk ? `under a copy of shard ${disk[0].i}` : "not reported", disk && disk[1] > 0.85 ? "var(--crit-ink)" : null, disk ? () => { sel = disk[0].i; lens = "disk"; lensSeg.set("disk"); draw(); } : null)));

    clear(barBox, lensSeg, legend(), h("div", { class: "grow" }), h("span", { class: "small muted" }, `cluster map epoch ${fmt.n(c.slot_epoch)} · every 5 s`));

    // ---- the shards
    const hit = failed.size ? blast(m, failed) : null;
    const fate = new Map();
    if (hit) { for (const s of hit.serving) fate.set(s.i, "serving"); for (const s of hit.failover) fate.set(s.i, "failover"); for (const s of hit.noWrites) fate.set(s.i, "noWrites"); for (const s of hit.dark) fate.set(s.i, "dark"); }
    const tiles = m.shards.map((sh, k) => {
      const v = vals[k], b = bin(v), was = minuteAgo(lens, sh.i);
      const delta = v != null && was != null && was > 0 ? (v - was) / was : null;
      const t = h("button", {
        class: "fleet-tile" + (sel === sh.i ? " sel" : "") + (fate.has(sh.i) ? " fate-" + fate.get(sh.i) : "") + (sh.status !== "ok" ? " bad" : ""),
        style: { background: b == null ? "var(--surface-2)" : RAMP[b], color: b != null && b >= 4 ? "#fff" : "var(--text)" },
        "aria-label": `Shard ${sh.i}: ${L.label} ${v == null ? "not measured" : L.fmt(v)}, ${fmt.plural(sh.copies.length, "copy", "copies")}, ${sh.status}`,
        onclick: () => { sel = sel === sh.i ? null : sh.i; draw(); },
        onpointerenter: () => hoverOn({ shard: sh.i }),
        onfocus: () => hoverOn({ shard: sh.i }),
      },
      h("span", { class: "fleet-tile-top" }, h("span", {}, sh.i), delta != null && Math.abs(delta) >= 0.1 ? h("span", { class: "fleet-delta" }, (delta > 0 ? "▲" : "▼") + fmt.pct(Math.abs(delta), 0)) : null),
      h("b", {}, v == null ? "–" : L.fmt(v)),
      h("span", { class: "fleet-dots" }, sh.copies.map((cp) => { const cb = copyBin(L.copy(cp)); return h("span", { class: "fleet-dot" + (cp.primary ? " p" : "") + (cp.down || failed.has(cp.host) ? " down" : ""), style: { background: cp.down ? "var(--crit)" : cb == null ? "var(--surface-3)" : RAMP[cb] } }); })));
      tileEls.set(sh.i, t);
      return t;
    });
    const shardsCard = card({ title: "Shards", sub: "a tile per shard, its colour its worst copy; a dot per copy, the round one its primary", body: h("div", { class: "fleet-grid" + (m.shards.length <= 24 ? " big" : ""), style: { "--tile": m.shards.length <= 8 ? "150px" : m.shards.length <= 24 ? "100px" : m.shards.length <= 96 ? "68px" : "54px" }, onpointerleave: () => hoverOn(null) }, tiles) });

    // ---- the machines
    const one = m.hosts.every((x) => x.copies.length === 1);
    const machines = m.hosts.map((mh) => {
      const isFailed = failed.has(mh.host);
      const b = h("button", {
        class: "fleet-host" + (isFailed ? " failed" : ""),
        "aria-pressed": String(isFailed), "aria-label": `${mh.host}: ${fmt.plural(mh.copies.length, "copy", "copies")}${isFailed ? ", failed in this what-if" : ""}. Click to ${isFailed ? "restore" : "fail"} it.`,
        onclick: () => { if (isFailed) failed.delete(mh.host); else failed.add(mh.host); draw(); },
        onpointerenter: () => hoverOn({ host: mh.host }),
        onfocus: () => hoverOn({ host: mh.host }),
      },
      h("span", { class: "fleet-cells" }, mh.copies.map((cp) => { const cb = copyBin(L.copy(cp)); return h("span", { class: "fleet-cell" + (cp.primary ? " p" : "") + (cp.down ? " down" : ""), title: `shard ${cp.shard} ${cp.primary ? "primary" : "replica"}`, style: { background: cp.down ? "var(--crit)" : cb == null ? "var(--surface-3)" : RAMP[cb] } }); })),
      h("span", { class: "mono fleet-host-name" }, mh.host),
      m.hosts.length <= 12 ? h("span", { class: "fleet-host-sub" }, `${fmt.plural(mh.copies.length, "copy", "copies")}${mh.copies.some((x) => x.primary) ? `, ${fmt.plural(mh.copies.filter((x) => x.primary).length, "primary", "primaries")}` : ""}`) : null);
      hostEls.set(mh.host, b);
      tipOn(b, () => h("div", {}, h("b", {}, mh.host), mh.copies.map((cp) => h("div", { class: "small" }, `shard ${cp.shard} ${cp.primary ? "primary" : "replica"} · ${short(cp.url)} · ${L.label} ${L.copy(cp) == null ? "–" : L.fmt(L.copy(cp))}${cp.down ? " · down" : ""}`))));
      return b;
    });
    const hostsCard = card({ title: one ? "Servers" : "Machines", sub: one ? "every copy has a server of its own · click to fail one" : "copies by the host in their URL · click to fail one", tools: failed.size ? [h("button", { class: "btn sm", onclick: () => { failed.clear(); draw(); } }, icon("refresh"), "Restore all")] : null, body: h("div", { class: "fleet-hosts" + (m.hosts.length <= 12 ? " big" : ""), onpointerleave: () => hoverOn(null) }, machines) });

    // ---- the side: what if, the shard, the advice
    const side = [];
    if (hit) {
      const line = (n, text, lv) => h("div", { class: "row nw", style: { gap: "8px", alignItems: "baseline" } }, h("b", { style: { minWidth: "28px", textAlign: "right", color: lv } }, fmt.n(n)), h("span", {}, text));
      const list = (xs) => xs.length ? " (" + xs.slice(0, 8).map((s) => s.i).join(", ") + (xs.length > 8 ? "…" : "") + ")" : "";
      side.push(card({ cls: "fleet-blast" + (hit.dark.length || hit.noWrites.length ? " bad" : ""), title: `If ${failed.size === 1 ? [...failed][0] : fmt.plural(failed.size, "machine")} failed`, sub: "a what-if: nothing is stopped",
        tools: [h("button", { class: "btn sm", onclick: () => { failed.clear(); draw(); } }, "Restore")],
        body: h("div", { class: "stack", style: { gap: "6px" } },
          hit.dark.length ? line(hit.dark.length, "shards would go dark: no copy left to read or write" + list(hit.dark), "var(--crit-ink)") : null,
          hit.noWrites.length ? line(hit.noWrites.length, "shards would stop taking writes: too few copies for a majority" + list(hit.noWrites), "var(--crit-ink)") : null,
          hit.failover.length ? line(hit.failover.length, "shards would lose their primary; with failover on, a replica takes over in a few seconds" + list(hit.failover), "var(--warn)") : null,
          hit.serving.length ? line(hit.serving.length, "shards would keep serving with a copy fewer" + list(hit.serving), "var(--good-ink)") : null,
          h("div", { class: "small muted" }, `${fmt.plural(m.shards.length - hit.dark.length - hit.noWrites.length - hit.failover.length - hit.serving.length, "other shard")} would not notice.`)) }));
    }
    const s = sel != null ? m.shards.find((x) => x.i === sel) : null;
    if (s) {
      const series = (history.get(lens)?.get(s.i) || []).map(([, v]) => v).filter((v) => v != null);
      const rows = s.copies.slice().sort((a, b) => (b.primary ? 1 : 0) - (a.primary ? 1 : 0)).map((cp) => h("tr", {},
        h("td", {}, cp.down ? pill("down", "down") : cp.primary ? pill("ok", "primary") : cp.in_sync ? pill("ok", "in sync") : cp.copying ? pill("info", "copying") : pill("warn", cp.behind != null ? `${fmt.si(cp.behind)} behind` : cp.role)),
        h("td", { class: "mono ellipsis", style: { maxWidth: "180px" }, title: cp.url }, short(cp.url)),
        h("td", { class: "num" }, fmt.ms(cp.load?.p99_ms)), h("td", { class: "num" }, fmt.rate(cp.load?.qps)), h("td", { class: "num" }, cp.load?.cpu != null ? fmt.pct(cp.load.cpu / Math.max(1, cp.cores || 1), 0) : "–"),
        h("td", { class: "num" }, fmt.pct(cp.load?.disk_used_ratio, 0)), h("td", { class: "num" }, fmt.si(cp.docs))));
      side.push(card({ title: `Shard ${s.i}`, sub: `${fmt.n(s.slots)} slots · ${fmt.pct(s.slots / SLOTS)} of the rows`,
        tools: [h("button", { class: "btn sm ghost", "aria-label": "Close the shard", onclick: () => { sel = null; draw(); } }, icon("x"))],
        body: h("div", { class: "stack", style: { gap: "10px" } },
          series.length > 1 ? h("div", {}, h("div", { class: "small muted" }, `${L.label} since this page opened`), sparkline(series, { width: 320, height: 40 })) : null,
          h("div", { style: { overflowX: "auto" } }, h("table", { class: "t compact" }, h("thead", {}, h("tr", {}, ["", "copy", "p99", "searches", "CPU", "disk", "rows"].map((x, i) => h("th", { class: i > 1 ? "num" : "" }, x)))), h("tbody", {}, rows))),
          h("div", { class: "row", style: { gap: "6px" } }, h("a", { class: "btn sm", href: "#/cluster" }, icon("layers"), "Change it in Cluster"), s.primary ? h("a", { class: "btn sm", href: "#/profile?tab=cpu&node=" + encodeURIComponent(s.primary.url) }, icon("flame"), "Profile the primary") : null)) }));
    }
    const advice = advise(m, c, managed);
    side.push(card({ title: "Advisor", sub: advice.length ? fmt.plural(advice.length, "thing") + " to look at" : "nothing to change",
      body: advice.length ? h("div", { class: "stack", style: { gap: "8px" } }, advice.slice(0, 8).map((a) => h("div", { class: "fleet-advice " + a.sev, onpointerenter: () => { if (a.shards.length) hoverOn({ shard: a.shards[0] }); }, onpointerleave: () => hoverOn(null) },
        h("div", { class: "row nw", style: { gap: "8px", alignItems: "flex-start" } }, h("span", { class: "fleet-sev " + a.sev }), h("div", { class: "grow" }, h("b", {}, a.title), h("div", { class: "small dim" }, a.why))),
        a.action ? h("div", { style: { paddingLeft: "16px", marginTop: "6px" } }, a.action.go ? h("a", { class: "btn sm", href: a.action.go }, a.action.label) : h("button", { class: "btn sm primary", onclick: a.action.run }, a.action.label)) : null)),
        advice.length > 8 ? h("div", { class: "small muted" }, `and ${advice.length - 8} more`) : null)
        : empty("okc", "Nothing to change", "Every copy answers, the replicas keep up, the disks have room and the slots are even.") }));

    clear(mainBox, h("div", { class: "fleet-main" }, h("div", { class: "fleet-left" }, shardsCard, hostsCard), h("div", { class: "fleet-side" }, side)));
    applyHover();
  }

  async function loadKube() {
    if (!isCoordinator()) return;
    try { kube = await api("/v1/kube"); } catch { kube = null; }
  }

  draw();
  loadKube().then(draw);
  if (state.cluster) remember(model(state.cluster));
  const offs = [
    bus.on("cluster", () => { remember(model(state.cluster)); draw(); }),
    bus.on("health", () => { if (!state.cluster) draw(); }),
  ];
  refreshCluster();
  return () => offs.forEach((f) => f());
}
