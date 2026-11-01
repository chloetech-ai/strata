// Control center: the whole cluster as one desired state. Its shape (shards
// and the copies of each, dragged by their handles), every setting of its
// StrataCluster with where the value comes from and what it does, what the
// change would do to each copy's load, disk and survival on the cluster's
// own numbers, the spec as it would read, and the operator's plan, checked
// by the coordinator before anything is applied. A cluster of machines of
// your own changes shape through the admin calls it plans and runs one at a
// time; its settings are the flags each server was started with.

import { h, icon, clear, fmt, card, api, toast, confirm, bus, seg, toggle, store, every, debounce } from "../core.js";
import { state, isCoordinator, refreshCluster } from "../data.js";

export default {
  id: "control", title: "Control center", icon: "sliders", group: "Admin",
  keywords: "control center desired state spec settings configure scale shards copies replicas coordinators plan apply diff yaml kubernetes operator volume storage resources cpu memory image version upgrade args flags hedge failover acks",
  render,
};

const SLOTS = 4096;
const clone = (x) => JSON.parse(JSON.stringify(x ?? null));
const same = (a, b) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
const plural = (n, w, ws) => `${fmt.n(n)} ${n === 1 ? w : ws || w + "s"}`;
const short = (url) => (url || "").replace(/^https?:\/\//, "");

/** The spec's fields as the operator reads them when the spec leaves them out. */
const DEFAULTS = {
  image: "strata-server:dev", imagePullPolicy: null, shards: 1, replicas: 3, coordinators: 1,
  storage: { size: "10Gi", storageClassName: null }, schema: null, demo: null, failoverAfterMs: 3000, writeAcks: "all",
  tokenSecret: null, clusterTokenSecret: null, resources: null, coordinatorResources: null, args: [], coordinatorArgs: [],
};

/** `raw` with every field the operator defaults filled in. */
function full(raw) {
  const out = clone(DEFAULTS);
  for (const [k, v] of Object.entries(raw || {})) {
    if (k === "storage" && v && typeof v === "object") out.storage = { ...out.storage, ...v };
    else out[k] = clone(v);
  }
  return out;
}

/** The merge patch (RFC 7386) that takes `a` to `b`. */
function patchOf(a, b) {
  const out = {};
  for (const k of new Set([...Object.keys(a || {}), ...Object.keys(b || {})])) {
    const x = a?.[k], y = b?.[k];
    if (same(x, y)) continue;
    if (y === undefined || y === null) out[k] = null;
    else if (x && y && typeof x === "object" && typeof y === "object" && !Array.isArray(x) && !Array.isArray(y)) out[k] = patchOf(x, y);
    else out[k] = clone(y);
  }
  return out;
}

// ------------------------------------------------------------ quantities
const UNITS = { "": 1, Ki: 2 ** 10, Mi: 2 ** 20, Gi: 2 ** 30, Ti: 2 ** 40, Pi: 2 ** 50, k: 1e3, M: 1e6, G: 1e9, T: 1e12, P: 1e15, m: 1e-3 };
function quantity(q) {
  const m = /^(\d+(?:\.\d+)?)(Ki|Mi|Gi|Ti|Pi|k|M|G|T|P|m)?$/.exec(String(q ?? "").trim());
  return m ? Number(m[1]) * UNITS[m[2] || ""] : null;
}
const cores = (q) => (q == null ? null : quantity(q));

// ------------------------------------------------------------ args
/** Splits a command line into words, as a shell would (quotes, no expansion). */
function words(text) {
  const out = []; let cur = null, q = null;
  for (const c of String(text || "")) {
    if (q) { if (c === q) q = null; else cur += c; continue; }
    if (c === "'" || c === '"') { q = c; cur = cur ?? ""; continue; }
    if (/\s/.test(c)) { if (cur != null) out.push(cur); cur = null; continue; }
    cur = (cur ?? "") + c;
  }
  if (cur != null) out.push(cur);
  return out;
}
const quote = (w) => (/^[\w@%+=:,./-]+$/.test(w) ? w : `'${w.replace(/'/g, `'\\''`)}'`);
const line = (argv) => (argv || []).map(quote).join(" ");

/** The value of `--flag` in `argv`: its word, true for a switch, undefined if absent. */
function argOf(argv, flag, bool) {
  for (let i = 0; i < (argv || []).length; i++) {
    const a = argv[i];
    if (a === flag) return bool ? true : argv[i + 1];
    if (a.startsWith(flag + "=")) return bool ? a.slice(flag.length + 1) !== "false" : a.slice(flag.length + 1);
  }
  return undefined;
}
function withArg(argv, flag, v, bool) {
  const out = [];
  for (let i = 0; i < (argv || []).length; i++) {
    const a = argv[i];
    if (a === flag) { if (!bool) i++; continue; }
    if (a.startsWith(flag + "=")) continue;
    out.push(a);
  }
  if (bool ? v : v !== undefined && v !== null && v !== "") out.push(...(bool ? [flag] : [flag, String(v)]));
  return out;
}

// ------------------------------------------------------------ paths
const getPath = (o, path) => path.split(".").reduce((x, k) => (x == null ? undefined : x[k]), o);
function setPath(o, path, v) {
  const ks = path.split(".");
  const stack = [o];
  let x = o;
  for (const k of ks.slice(0, -1)) { if (x[k] == null || typeof x[k] !== "object") x[k] = {}; x = x[k]; stack.push(x); }
  const last = ks[ks.length - 1];
  if (v === undefined || v === null || v === "") delete x[last]; else x[last] = v;
  // Objects left empty go, back to the top (a field the operator defaults to null).
  for (let i = ks.length - 1; i > 0; i--) {
    if (Object.keys(stack[i]).length) break;
    const parent = stack[i - 1];
    if (i === 1 && (ks[0] === "storage")) break;
    if (i === 1) parent[ks[0]] = null; else delete parent[ks[i - 1]];
  }
}

// ------------------------------------------------------------ the cluster now
/** What /v1/cluster says, as the numbers the estimates start from. */
function live(c) {
  const shards = (c?.shards || []).map((sh, i) => {
    const copies = (sh.replicas || []).map((r) => {
      const l = r.detail?.load || {};
      const total = l.disk_total_bytes ?? (l.disk_free_bytes != null && l.disk_used_ratio != null && l.disk_used_ratio < 1 ? l.disk_free_bytes / (1 - l.disk_used_ratio) : null);
      return {
        url: r.url, primary: !!r.takes_writes, down: r.status === "down", role: r.role, docs: r.docs ?? 0,
        qps: l.qps ?? 0, p99: l.p99_ms, cpu: l.cpu, cores: r.detail?.system?.cores, rss: l.rss_bytes, file: l.file_bytes,
        diskTotal: total, diskUsed: total != null && l.disk_used_ratio != null ? total * l.disk_used_ratio : null,
      };
    });
    copies.sort((a, b) => (b.primary - a.primary));
    return { i, copies, slots: sh.slots_held ?? 0, rows: Math.max(0, ...copies.map((x) => x.docs)) };
  });
  const copies = shards.flatMap((s) => s.copies);
  const mean = (xs) => { const v = xs.filter((x) => x != null && !Number.isNaN(x)); return v.length ? v.reduce((a, b) => a + b, 0) / v.length : null; };
  const max = (xs) => { const v = xs.filter((x) => x != null && !Number.isNaN(x)); return v.length ? Math.max(...v) : null; };
  const per = shards.map((s) => s.copies.length);
  const R = per.length ? Math.round(mean(per)) : 0;
  return {
    shards, S: shards.length, R, servers: copies.length,
    // A search asks every shard once: a shard's copies share the cluster's searches.
    qps: mean(shards.map((s) => s.copies.reduce((a, x) => a + x.qps, 0))) ?? 0,
    p99: max(copies.map((x) => x.p99)), cpu: mean(copies.map((x) => x.cpu)), cores: mean(copies.map((x) => x.cores)),
    rss: mean(copies.map((x) => x.rss)), file: mean(copies.map((x) => x.file)),
    diskUsed: mean(copies.map((x) => x.diskUsed)), diskTotal: mean(copies.map((x) => x.diskTotal)),
    rows: shards.reduce((a, s) => a + s.rows, 0), down: copies.filter((x) => x.down).length,
    hedge: c?.hedge_ms, epoch: c?.slot_epoch,
    // As the primaries say: how they acknowledge writes.
    acks: (c?.shards || []).flatMap((sh) => sh.replicas || []).map((r) => r.detail?.health?.replication).find((x) => x?.role === "primary")?.write_acks,
  };
}

/** Each copy's share of the work after the change: reads spread over the
 *  copies of a shard, rows and their bytes over the shards. */
function after(L, S, R) {
  const kS = L.S ? L.S / S : 1, kR = L.R ? L.R / R : 1;
  return {
    qps: L.R ? (L.qps / R) : null,
    cpu: L.cpu != null ? L.cpu * kS * kR : null,
    rows: L.rows / S,
    file: L.file != null ? L.file * kS : null,
    diskUsed: L.diskUsed != null ? L.diskUsed * kS : null,
  };
}

// ------------------------------------------------------------ settings
const steps = (list, v, d) => { let i = list.findIndex((x) => same(x, v)); if (i < 0) i = d > 0 ? -1 : list.length; return list[Math.max(0, Math.min(list.length - 1, i + d))]; };
const sec = (ms) => (ms % 1000 ? (ms / 1000).toFixed(1) : ms / 1000) + " s";
const qty = (v, none) => (v == null ? none : String(v));
const pct = (r) => (r > 0 && r < 0.001 ? "under 0.1%" : fmt.pct(r, r < 0.1 ? 1 : 0));
const every_ = (s) => (s < 60 ? s + " s" : s < 3600 ? s / 60 + " min" : s / 3600 + " h");

/**
 * Every setting the view edits: where it lives (`spec:path`, `args:--flag`
 * for every server, `cargs:--flag` for every coordinator), how it is
 * picked, what each value does, and what it takes to apply.
 */
const SETTINGS = [
  { g: "Durability", label: "Write acknowledged by", at: "spec:writeAcks", type: "seg", opts: [["all", "every copy in step"], ["majority", "a majority"], ["primary", "the primary"]],
    effect: (v) => ({ all: "A write is answered once every copy in step holds it, and never fewer than a majority.", majority: "A write is answered once most copies hold it: it survives any minority failing.", primary: "A write is answered once the primary has it: a failover can lose the last writes." })[v],
    warn: (v, x) => (v === "primary" && x.R > 1 ? "a failover may lose the writes its replicas had not received" : null), takes: "servers" },
  { g: "Durability", label: "Fail over after", at: "spec:failoverAfterMs", type: "step", list: [0, 1000, 2000, 3000, 5000, 10000, 30000], show: (v) => (v ? sec(v) : "by hand"),
    effect: (v) => (v ? `A replica that hears nothing from its primary for ${sec(v)} takes over: writes resume a few seconds after a primary dies; reads go on from the other copies.` : "No automatic failover: a primary that dies stops its shard's writes until someone promotes a copy."),
    warn: (v, x) => (!v && x.R > 1 ? "failover by hand: a dead primary stops its shard's writes" : v && v < 1500 ? "a network blip this short elects a new primary for nothing" : null), takes: "servers" },
  { g: "Durability", label: "Ship log", at: "args:--ship-log-mb", type: "step", list: [16, 32, 64, 128, 256, 512, 1024], def: 64, show: (v) => v + " MiB",
    effect: (v) => `The latest ${v} MiB of writes stay in each primary's memory to ship; a replica further behind is sent a whole copy of its primary instead.`, takes: "servers" },
  { g: "Durability", label: "Compare replicas every", at: "args:--verify-every-s", type: "step", list: [60, 300, 600, 1800, 3600], def: 600, show: (v) => every_(v),
    effect: (v) => `Each primary compares its replicas' rows with its own every ${every_(v)}, and sends one that differs a copy.`, takes: "servers" },

  { g: "Search", label: "Hedge after", at: "cargs:--hedge-ms", type: "step", list: [10, 20, 30, 50, 75, 100, 150, 200, 500, 1000], def: 100, show: (v) => v + " ms",
    effect: (v, x) => `When a shard's copy has not answered after ${v} ms, the coordinator asks another copy of it.` + (x.L.p99 != null ? (v > x.L.p99 ? ` The slowest copy's p99 is ${fmt.ms(x.L.p99)} now: it rarely fires.` : ` The slowest copy's p99 is ${fmt.ms(x.L.p99)} now: it fires for the slowest searches.`) : ""),
    warn: (v, x) => (x.R < 2 ? "one copy a shard: there is no other copy to hedge to" : null), takes: "coordinators" },
  { g: "Search", label: "Give up on a shard after", at: "cargs:--shard-timeout-ms", type: "step", list: [undefined, 100, 250, 500, 1000, 2000, 5000], show: (v) => (v ? v + " ms" : "the deadline"),
    effect: (v) => (v ? `A shard that has not answered in ${v} ms fails the search, or is left out of a partial answer.` : "A search waits for every shard until its deadline."), takes: "coordinators" },
  { g: "Search", label: "Partial answers", at: "cargs:--partial", type: "toggle", bool: true,
    effect: (v) => (v ? "A search answers with the shards that answered, and names the ones missing." : "A search fails when a shard has no copy answering."), takes: "coordinators" },
  { g: "Search", label: "Default deadline", at: "cargs:--timeout-ms", type: "step", list: [1000, 2000, 5000, 10000, 30000, 60000], def: 5000, show: (v) => sec(v),
    effect: (v) => `A query that names no timeout_ms gives up after ${sec(v)}.`, takes: "coordinators" },

  { g: "Limits", label: "Queries at once, per server", at: "args:--max-concurrent", type: "step", list: [8, 16, 32, 64, 128, 256, 512], def: 64, show: String,
    effect: (v) => `Each server runs ${v} queries at once; more wait for a slot until their deadline.`, takes: "servers" },
  { g: "Limits", label: "Log queries slower than", at: "args:--slow-ms", type: "step", list: [0, 50, 100, 200, 500, 1000, 2000, 5000], def: 1000, show: (v) => (v ? v + " ms" : "off"),
    effect: (v) => (v ? `Queries and writes that take ${v} ms or more are logged with their SQL and kept with their profiles.` : "No query is logged as slow."), takes: "servers" },
  { g: "Limits", label: "Most rows a query reaches", at: "args:--max-result-window", type: "step", list: [1000, 10000, 100000, 1000000, 0], def: 10000, show: (v) => (v ? fmt.n(v) : "no limit"),
    effect: (v) => (v ? `LIMIT plus OFFSET up to ${fmt.n(v)}; a query that would reach more fails.` : "A query may page through every row."), takes: "servers" },

  { g: "Capacity", label: "Cores per server", at: "spec:resources.requests.cpu", type: "step", list: [undefined, "250m", "500m", "1", "2", "4", "8", "16", "32"], show: (v) => qty(v, "no request"),
    effect: (v, x) => {
      const now = x.L.cpu, then = x.A.cpu;
      const use = now != null ? ` Each copy uses about ${fmt.si(now)} cores now${then != null && Math.abs(then - now) > 0.01 ? `, about ${fmt.si(then)} after this change` : ""}.` : "";
      return (v ? `Kubernetes keeps ${v} cores for each server.` : "Kubernetes keeps no cores for the servers: they get what is left on their node.") + use;
    },
    warn: (v, x) => (v && x.A.cpu != null && x.A.cpu > cores(v) * 0.8 ? `each copy would need about ${fmt.si(x.A.cpu)} cores of the ${v} it is given` : null), takes: "servers" },
  { g: "Capacity", label: "Memory per server", at: "spec:resources.requests.memory", also: ["spec:resources.limits.memory"], type: "step", list: [undefined, "512Mi", "1Gi", "2Gi", "4Gi", "8Gi", "16Gi", "32Gi", "64Gi", "128Gi"], show: (v) => qty(v, "no request"),
    effect: (v, x) => (v ? `Kept for each server and its limit: one that uses more is restarted by Kubernetes.` : "No request or limit: a server may use its node's memory.") + (x.L.rss != null ? ` Each copy holds about ${fmt.bytes(x.L.rss)} now, for a database of ${fmt.bytes(x.L.file)}.` : ""),
    warn: (v, x) => (v && x.L.rss != null && quantity(v) < x.L.rss * 1.2 ? `a copy holds about ${fmt.bytes(x.L.rss)} now: ${v} would restart it` : null), takes: "servers" },
  { g: "Capacity", label: "Volume per copy", at: "spec:storage.size", type: "step", list: ["1Gi", "2Gi", "5Gi", "10Gi", "20Gi", "50Gi", "100Gi", "200Gi", "500Gi", "1Ti", "2Ti", "4Ti"], grows: true, show: String,
    effect: (v, x) => {
      const used = x.A.file ?? x.A.diskUsed;
      return used != null ? `Each copy would hold about ${fmt.bytes(used)}: ${pct(used / quantity(v))} of ${v}. Merges need room to write the new segment beside the old ones.` : `Each copy's volume. It only grows, in place, when its storage class allows it.`;
    },
    warn: (v, x) => { const used = x.A.file ?? x.A.diskUsed; return used != null && used / quantity(v) > 0.8 ? `each copy's volume would be ${fmt.pct(used / quantity(v))} full` : null; }, takes: "volumes" },
  { g: "Capacity", label: "Storage class", at: "spec:storage.storageClassName", type: "text", placeholder: "the cluster's default",
    effect: (v) => (v ? `New volumes are made of ${v}; the copies that run keep theirs.` : "New volumes take the cluster's default class."), takes: "new volumes" },
  { g: "Capacity", label: "Merges at once", at: "args:--merge-threads", type: "step", list: [undefined, 1, 2, 3, 4, 6, 8], show: (v) => (v ? String(v) : "half the cores, 1 to 4"),
    effect: (v) => (v > 2 ? "Segments merge sooner, but merges take cores and disk from searches." : "Merges stay out of the searches' way; segments pile up for a while after a large import."), takes: "servers" },
  { g: "Capacity", label: "Merge write limit", at: "args:--merge-write-mb-per-sec", type: "step", list: [undefined, 25, 50, 100, 200, 400], show: (v) => (v ? v + " MiB/s" : "no limit"),
    effect: (v) => (v ? `Merges write at most ${v} MiB/s, so they cannot crowd the searches off the disk.` : "Merges write as fast as the disk lets them."), takes: "servers" },

  { g: "Coordinators", label: "Coordinators", at: "spec:coordinators", type: "step", list: [1, 2, 3, 4, 6, 8, 12, 16], show: String,
    effect: (v) => (v > 1 ? `${v} coordinators behind the cluster's Service share the searches; one can go without the cluster going.` : "One coordinator: while it restarts, the cluster answers no one."),
    warn: (v) => (v < 2 ? "one coordinator: the cluster is unreachable while it restarts" : null), takes: "coordinators" },
  { g: "Coordinators", label: "Cores per coordinator", at: "spec:coordinatorResources.requests.cpu", type: "step", list: [undefined, "250m", "500m", "1", "2", "4", "8"], show: (v) => qty(v, "no request"),
    effect: (v) => (v ? `Kubernetes keeps ${v} cores for each coordinator.` : "Kubernetes keeps no cores for the coordinators."), takes: "coordinators" },
  { g: "Coordinators", label: "Memory per coordinator", at: "spec:coordinatorResources.requests.memory", also: ["spec:coordinatorResources.limits.memory"], type: "step", list: [undefined, "256Mi", "512Mi", "1Gi", "2Gi", "4Gi", "8Gi"], show: (v) => qty(v, "no request"),
    effect: (v) => (v ? "Kept for each coordinator and its limit: it merges the shards' answers." : "No request or limit."), takes: "coordinators" },

  { g: "Access", label: "Client token", at: "spec:tokenSecret", type: "text", placeholder: "a Secret with key token",
    effect: (v) => (v ? `Clients and admins send the token in Secret ${v}, or an API key, to the coordinators.` : "No token: anything that reaches the Service may read, write and change the cluster."),
    warn: (v) => (!v ? "no token: anything that reaches the pods may read and write" : null), takes: "servers" },
  { g: "Access", label: "Cluster token", at: "spec:clusterTokenSecret", type: "text", placeholder: "a second Secret",
    effect: (v) => (v ? `The servers take only the token in Secret ${v} from each other: the client token cannot replicate or move slots.` : "The client token also makes the calls between servers."), takes: "servers" },

  { g: "Version", label: "Image", at: "spec:image", type: "text", placeholder: DEFAULTS.image,
    effect: () => "Every server and coordinator restarts on it, one at a time; a primary hands its shard over first.", takes: "servers" },
  { g: "Version", label: "Pull the image", at: "spec:imagePullPolicy", type: "seg", opts: [[undefined, "default"], ["IfNotPresent", "if not present"], ["Always", "always"], ["Never", "never"]],
    effect: (v) => ({ IfNotPresent: "A node pulls the image when it does not have it.", Always: "A pod pulls the image every time it starts.", Never: "The image must already be on each node." })[v] || "Kubernetes pulls a :latest image every time, others when a node lacks them.", takes: "servers" },

  { g: "More arguments", label: "Every server", at: "argv:args", type: "argv", placeholder: "--flag value …",
    effect: () => "The arguments every server is started with, besides those the operator gives (strata-server --help lists them).", takes: "servers" },
  { g: "More arguments", label: "Every coordinator", at: "argv:coordinatorArgs", type: "argv", placeholder: "--flag value …",
    effect: () => "The arguments every coordinator is started with.", takes: "coordinators" },
];

const TAKES = {
  servers: "restarts every server, one at a time",
  coordinators: "restarts the coordinators",
  volumes: "grows each volume in place",
  "new volumes": "applies to new volumes",
};

function value(d, st) {
  const [where, rest] = st.at.split(/:(.*)/s);
  if (where === "spec") return getPath(d, rest) ?? undefined;
  if (where === "argv") return d[rest] || [];
  const v = argOf(d[where === "args" ? "args" : "coordinatorArgs"], rest, st.bool);
  if (st.bool) return !!v;
  if (v === undefined) return undefined;
  return typeof (st.list || []).find((x) => x != null) === "number" && !Number.isNaN(Number(v)) ? Number(v) : v;
}
function assign(d, st, v) {
  const [where, rest] = st.at.split(/:(.*)/s);
  if (where === "spec") {
    if (rest === "storage.size") { d.storage = { ...(d.storage || {}), size: v }; return; }
    setPath(d, rest, v);
    for (const p of st.also || []) setPath(d, p.slice(5), v);
    return;
  }
  if (where === "argv") { d[rest] = v; return; }
  const key = where === "args" ? "args" : "coordinatorArgs";
  d[key] = withArg(d[key], rest, st.def !== undefined && v === st.def ? undefined : v, st.bool);
}
/** The value shown: what is set, or what the server does without it. */
const shown = (st, v) => (st.type === "seg" ? (st.opts.find((o) => same(o[0], v)) || [, String(v)])[1] : st.type === "toggle" ? (v ? "on" : "off") : st.show ? st.show(v ?? st.def) : v);

// ------------------------------------------------------------ YAML
function yaml(spec) {
  const out = [];
  // A string plain unless YAML would read it as something else.
  const scalar = (v) => (typeof v === "string" ? (/^[\w./:@-]+$/.test(v) && !/^(true|false|null|yes|no|on|off|~)$/i.test(v) && !/^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$/.test(v) && !v.startsWith("-") ? v : JSON.stringify(v)) : JSON.stringify(v));
  const walk = (o, ind) => {
    for (const [k, v] of Object.entries(o)) {
      if (v === null || v === undefined || (Array.isArray(v) && !v.length)) continue;
      if (k === "schema" && ind === "  ") { out.push(`${ind}schema: {…}   # the first table's, ${Object.keys(v.fields || {}).length} fields`); continue; }
      if (Array.isArray(v)) out.push(`${ind}${k}: [${v.map(scalar).join(", ")}]`);
      else if (typeof v === "object") { if (!Object.values(v).some((x) => x != null)) continue; out.push(`${ind}${k}:`); walk(v, ind + "  "); }
      else out.push(`${ind}${k}: ${scalar(v)}`);
    }
  };
  out.push("spec:");
  walk(spec, "  ");
  return out;
}
/** The lines of `b` against `a`: [mark, text] with " ", "-" and "+". */
function lineDiff(a, b) {
  const n = a.length, m = b.length, L = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) L[i][j] = a[i] === b[j] ? L[i + 1][j + 1] + 1 : Math.max(L[i + 1][j], L[i][j + 1]);
  const out = []; let i = 0, j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && a[i] === b[j]) { out.push([" ", a[i]]); i++; j++; }
    else if (i < n && (j === m || L[i + 1][j] >= L[i][j + 1])) { out.push(["-", a[i]]); i++; }
    else { out.push(["+", b[j]]); j++; }
  }
  return out;
}

// ------------------------------------------------------------ intents
const NUM = { a: 1, an: 1, one: 1, two: 2, three: 3, four: 4, five: 5, six: 6, seven: 7, eight: 8, nine: 9, ten: 10, twice: 2, double: 2 };
const num = (w) => NUM[String(w).toLowerCase()] ?? parseInt(w, 10);
const NW = "(\\d+|a|an|one|two|three|four|five|six|seven|eight|nine|ten)";

/** What a sentence asks the draft for: {text, apply(d)}, or null. */
function intent(text, d, L) {
  const t = text.trim().toLowerCase().replace(/[“”"]/g, "");
  if (!t) return null;
  let m;
  const out = (said, f) => ({ text: said, apply: f });
  const byArg = (key, flag, v) => (x) => { x[key] = withArg(x[key], flag, v); };
  if ((m = new RegExp(`(add|remove|drop)\\s+${NW}\\s+(more\\s+)?shards?`).exec(t))) {
    const k = num(m[2]) * (m[1] === "add" ? 1 : -1), n = Math.max(1, d.shards + k);
    return out(`${n} shards, from ${d.shards}`, (x) => { x.shards = n; });
  }
  if ((m = new RegExp(`(add|remove|drop)\\s+${NW}\\s+(more\\s+)?(copies|copy|replicas?)`).exec(t))) {
    const k = num(m[2]) * (m[1] === "add" ? 1 : -1), n = Math.max(1, Math.min(9, d.replicas + k));
    return out(`${n} copies of each shard, from ${d.replicas}`, (x) => { x.replicas = n; });
  }
  if ((m = new RegExp(`${NW}\\s+shards?`).exec(t)) && !/cop|replica/.test(t)) { const n = Math.max(1, num(m[1])); return out(`${n} shards, from ${d.shards}`, (x) => { x.shards = n; }); }
  if ((m = new RegExp(`${NW}\\s+(copies|replicas)`).exec(t))) { const n = Math.max(1, Math.min(9, num(m[1]))); return out(`${n} copies of each shard, from ${d.replicas}`, (x) => { x.replicas = n; }); }
  if ((m = new RegExp(`${NW}\\s+coordinators?`).exec(t))) { const n = Math.max(1, num(m[1])); return out(`${n} coordinators, from ${d.coordinators}`, (x) => { x.coordinators = n; }); }
  if (/surviv|lose a (server|copy|machine|node)|losing a (server|copy|machine|node)|high(ly)? availab|\bha\b/.test(t)) {
    return out(`at least 3 copies of each shard, writes on a majority, and failover after ${sec(d.failoverAfterMs || 3000)}`, (x) => { x.replicas = Math.max(3, x.replicas); if (x.writeAcks === "primary") x.writeAcks = "majority"; if (!x.failoverAfterMs) x.failoverAfterMs = 3000; });
  }
  if (/(double|twice).*(read|search|query|queries)|(read|search).*(capacity|throughput).*(double|twice|x2)/.test(t)) {
    const n = Math.min(9, d.replicas * 2);
    return out(`${n} copies of each shard, from ${d.replicas}: each copy serves half the searches it does now`, (x) => { x.replicas = n; });
  }
  if (/(double|twice).*(write|row|data|index)|(write|data).*(capacity|room).*(double|twice|x2)/.test(t)) {
    const n = d.shards * 2;
    return out(`${n} shards, from ${d.shards}: each holds half the rows it does now`, (x) => { x.shards = n; });
  }
  if ((m = /(\d+(?:\.\d+)?)\s*(gi|ti|gb|tb|g|t)\b/.exec(t)) && /volume|disk|storage|grow/.test(t)) {
    const size = m[1] + { gi: "Gi", gb: "Gi", g: "Gi", ti: "Ti", tb: "Ti", t: "Ti" }[m[2]];
    return out(`volumes of ${size}, from ${d.storage?.size}`, (x) => { x.storage = { ...(x.storage || {}), size }; });
  }
  if ((m = /(\d+(?:\.\d+)?)\s*(cores?|cpus?)/.exec(t))) {
    const v = m[1];
    if (/coordinator/.test(t)) return out(`${v} cores for each coordinator`, (x) => { setPath(x, "coordinatorResources.requests.cpu", v); });
    return out(`${v} cores for each server`, (x) => { setPath(x, "resources.requests.cpu", v); });
  }
  if ((m = /(\d+)\s*(gi|gb|g|mi|mb|m)\b.*(memory|ram)|(memory|ram).*?(\d+)\s*(gi|gb|g|mi|mb|m)\b/.exec(t))) {
    const n = m[1] || m[5], u = (m[2] || m[6])[0] === "g" ? "Gi" : "Mi", v = n + u;
    const at = /coordinator/.test(t) ? "coordinatorResources" : "resources";
    return out(`${v} of memory for each ${at === "resources" ? "server" : "coordinator"}`, (x) => { setPath(x, at + ".requests.memory", v); setPath(x, at + ".limits.memory", v); });
  }
  if ((m = /hedge.*?(\d+)\s*ms/.exec(t))) return out(`hedge after ${m[1]} ms`, byArg("coordinatorArgs", "--hedge-ms", m[1]));
  if ((m = /slow.*?(\d+)\s*ms|(\d+)\s*ms.*slow/.exec(t))) return out(`log queries slower than ${m[1] || m[2]} ms`, byArg("args", "--slow-ms", m[1] || m[2]));
  if (/partial/.test(t)) { const on = !/no partial|without partial|partial.*off/.test(t); return out(on ? "partial answers when a shard does not answer" : "no partial answers", (x) => { x.coordinatorArgs = withArg(x.coordinatorArgs, "--partial", on, true); }); }
  if (/fail ?over.*(by hand|off|manual)|no (automatic )?fail ?over/.test(t)) return out("failover by hand", (x) => { x.failoverAfterMs = 0; });
  if ((m = /fail ?over.*?(\d+(?:\.\d+)?)\s*(s|sec|seconds|ms)\b/.exec(t))) { const ms = Math.round(Number(m[1]) * (m[2] === "ms" ? 1 : 1000)); return out(`failover after ${sec(ms)}`, (x) => { x.failoverAfterMs = ms; }); }
  if (/(automatic|auto) fail ?over|fail ?over (on|automatically)/.test(t)) return out("failover after 3 s", (x) => { x.failoverAfterMs = 3000; });
  if (/(acks?|acknowledge).*majority|majority.*(ack|write)/.test(t)) return out("a write is answered once a majority holds it", (x) => { x.writeAcks = "majority"; });
  if ((m = /(?:upgrade|update|move|go|roll)\s+(?:to|onto)\s+(\S+)|(?:image|version)\s+(\S+)/.exec(t))) {
    let img = text.trim().split(/\s+/).pop().replace(/[“”"]/g, "");
    if (!/[:/@]/.test(img)) img = (d.image || DEFAULTS.image).replace(/:[^:/]*$/, "") + ":" + img;
    return out(`image ${img}, from ${d.image}`, (x) => { x.image = img; });
  }
  if (/(p99|latency|faster|slow)/.test(t) && L.p99 != null) {
    const n = Math.min(9, d.replicas + 1);
    return out(`${n} copies of each shard: each copy serves fewer searches, and the coordinator has another copy to hedge to`, (x) => { x.replicas = n; });
  }
  return null;
}

// ------------------------------------------------------------ the view
function render(page) {
  const head = h("div"), intentBox = h("div"), mainBox = h("div");
  page.append(head, intentBox, mainBox);
  if (!isCoordinator() && state.health) {
    clear(mainBox, card({ body: h("div", { class: "stack" }, h("div", {}, "This server is not a cluster: the control center changes the shards and copies a coordinator serves."), h("a", { class: "btn sm", href: "#/cluster", style: { alignSelf: "flex-start" } }, "Make a cluster")) }));
    return;
  }

  let kube = null;          // GET /v1/kube
  let base = null;          // the spec as it is, the operator's defaults filled in
  let hist = [], pos = 0;   // drafts: undo goes back through them
  let plan = null, planErr = null, planFor = null, planning = false;
  let applying = false, said = [], watching = false;
  let filter = "", onlyChanged = false;
  let urls = "";            // machines of your own: the new copies' gRPC URLs
  let run = null;           // machines of your own: the steps being run
  const managed = () => !!kube?.managed;
  const key = () => (managed() ? `control.draft.${kube.namespace}/${kube.name}` : "control.draft.own");
  const draft = () => hist[pos];
  const L = () => live(state.cluster);

  function reset(spec) {
    base = spec;
    const kept = store.get(key(), null);
    hist = [clone(base)]; pos = 0;
    if (kept && same(kept.base, base) && !same(kept.draft, base)) { hist.push(kept.draft); pos = 1; }
  }
  function set(f, then) {
    const d = clone(draft());
    f(d);
    if (same(d, draft())) return;
    hist = hist.slice(0, pos + 1);
    hist.push(d); pos++;
    changed(then);
  }
  function changed(then) {
    store.set(key(), { base, draft: draft() });
    draw();
    if (then) then();
    replan();
  }

  // ---------------------------------------------------------- loading
  async function load() {
    try { kube = await api("/v1/kube"); } catch (e) { kube = { managed: false, error: e }; }
    if (managed()) {
      const spec = full(kube.spec);
      if (!base || !same(spec, base)) reset(spec);
    } else if (state.cluster && !base) {
      const l = L();
      reset({ shards: l.S, replicas: l.R, coordinators: 1 });
    }
    drawHead(); draw(); replan();
  }

  // ---------------------------------------------------------- the plan
  const replan = debounce(async () => {
    if (!base || !draft()) return;
    if (!managed()) { plan = ownPlan(); planErr = null; drawPlan(); return; }
    const patch = patchOf(base, draft());
    if (!Object.keys(patch).length) { plan = null; planErr = null; planFor = null; drawPlan(); return; }
    const want = JSON.stringify(patch);
    if (want === planFor && plan) return;
    planning = true; planFor = want; drawPlan();
    try { const p = await api("/v1/admin/kube/plan", { body: { spec: patch } }); if (planFor === want) { plan = p; planErr = null; } }
    catch (e) { if (planFor === want) { plan = null; planErr = e.message; } }
    planning = false;
    drawPlan(); drawGuard();
  }, 250);

  async function apply() {
    const patch = patchOf(base, draft());
    const changes = plan?.changes || [];
    const ok = await confirm({
      title: `Change ${kube.namespace}/${kube.name}`,
      body: h("div", { class: "stack", style: { gap: "8px" } },
        h("div", {}, `The operator makes ${plural(changes.length, "change")} to the StrataCluster, one step at a time, while the cluster serves:`),
        h("ol", { class: "ctl-steps" }, (plan?.steps || []).map((s) => h("li", {}, s))),
        (plan?.warnings || []).length ? h("div", { class: "small", style: { color: "var(--serious)" } }, "Mind: " + plan.warnings.join("; ") + ".") : null),
      ok: "Apply",
    });
    if (!ok) return;
    applying = true; drawPlan();
    try {
      const j = await api("/v1/admin/kube", { body: { spec: patch } });
      toast("The operator has the new spec", "ok");
      store.set(key(), null);
      kube = j.managed ? j : kube;
      said = []; watching = true;
      reset(full(kube.spec));
      watch();
    } catch (e) {
      toast("Not applied: " + e.message, "bad", 8000);
    }
    applying = false;
    drawHead(); draw(); replan();
  }

  // The operator's progress after an apply: each step it says, until the
  // cluster is as the spec says.
  let stopWatch = null;
  function watch() {
    stopWatch?.();
    stopWatch = every(2000, async () => {
      try { kube = await api("/v1/kube"); } catch { return; }
      const m = kube.status?.message;
      if (m && said[said.length - 1] !== m) said.push(m);
      if (kube.settled && kube.status?.phase === "Ready" && said.length) { watching = false; stopWatch?.(); stopWatch = null; refreshCluster(); }
      drawHead(); drawPlan();
    });
  }

  // ---------------------------------------------------------- own machines
  /** The admin calls that take the cluster to the draft's shape. */
  function ownPlan() {
    const l = L(), d = draft();
    const given = words(urls.replace(/[,;]/g, " "));
    let next = 0;
    const take = () => given[next++];
    const out = [];
    const need = [];
    l.shards.forEach((sh, i) => {
      if (i >= d.shards) return;
      for (let k = sh.copies.length; k < d.replicas; k++) {
        const url = take();
        need.push(`shard ${i}, copy ${k + 1}`);
        out.push({ text: `${url ? short(url) : "a new server"} joins shard ${i}, and is sent a copy of its primary`, call: url ? ["POST", `/v1/admin/shards/${i}/replicas`, { url }] : null });
      }
    });
    for (let i = l.S; i < d.shards; i++) {
      const servers = [];
      for (let k = 0; k < d.replicas; k++) { servers.push(take()); need.push(`new shard ${i}, copy ${k + 1}`); }
      const ok = servers.every(Boolean);
      out.push({ text: `shard ${i} joins the cluster, served by ${ok ? servers.map(short).join(", ") : plural(d.replicas, "new server")}`, call: ok ? ["POST", "/v1/admin/shards", { servers }] : null });
    }
    if (d.shards > l.S) out.push({ text: `the slots even out over ${d.shards} shards: about ${fmt.n(SLOTS - Math.floor(SLOTS / d.shards) * l.S)} move, a run at a time, while the cluster serves`, call: ["POST", "/v1/admin/rebalance", {}] });
    // Fewer copies: the last replicas of each shard leave (never its primary).
    l.shards.forEach((sh, i) => {
      const extra = sh.copies.length - d.replicas;
      if (i >= d.shards || extra <= 0) return;
      const replicas = sh.copies.filter((c) => !c.primary);
      for (const c of replicas.slice(replicas.length - extra)) out.push({ text: `${short(c.url)} leaves shard ${i}`, call: ["DELETE", `/v1/admin/shards/${i}/replicas?url=${encodeURIComponent(c.url)}`] });
    });
    for (let i = l.S - 1; i >= d.shards; i--) {
      out.push({ text: `shard ${i} drains: its ${fmt.n(l.shards[i].slots)} slots move to the others`, call: ["POST", `/v1/admin/shards/${i}/drain`, {}], wait: true });
      out.push({ text: `shard ${i} leaves the cluster (its servers keep their data until you stop them)`, call: ["DELETE", `/v1/admin/shards/${i}`] });
    }
    const missing = need.length - Math.min(need.length, given.length);
    const warnings = [];
    if (d.replicas === 1) warnings.push("one copy a shard: losing its server loses the shard until it is back");
    if (d.replicas < l.R || d.shards < l.S) warnings.push("a server that leaves keeps its data: stop it yourself once it is out");
    return { steps: out, need, missing, given: given.length, warnings, refused: missing > 0 ? `give the gRPC URL of ${plural(missing, "more new server")}: each started with --seq-no and the cluster's schema` : null };
  }

  async function runOwn() {
    const p = ownPlan();
    if (p.refused || !p.steps.length) return;
    const ok = await confirm({ title: "Change the cluster", body: h("ol", { class: "ctl-steps" }, p.steps.map((s) => h("li", {}, s.text))), ok: "Run these steps" });
    if (!ok) return;
    run = p.steps.map((s) => ({ ...s, st: "wait" }));
    drawPlan();
    for (const s of run) {
      s.st = "run"; drawPlan();
      try {
        const [method, path, body] = s.call;
        await api(path, method === "DELETE" ? { method } : { body });
        if (s.wait) {
          for (let t = 0; ; t++) {
            await new Promise((r) => setTimeout(r, 1000));
            const m = await api("/v1/admin/moves").catch(() => null);
            const busy = m && (m.moving || (m.moves || []).some((x) => !x.done && !x.finished && x.state !== "done"));
            if (!busy || t > 3600) break;
            s.text2 = "slots moving…"; drawPlan();
          }
        }
        s.st = "ok";
      } catch (e) { s.st = "bad"; s.err = e.message; drawPlan(); toast("Stopped: " + e.message, "bad", 8000); refreshCluster(); return; }
      drawPlan();
      await refreshCluster();
    }
    toast("The cluster has its new shape", "ok");
    urls = "";
    reset({ shards: L().S, replicas: L().R, coordinators: 1 });
    run = null;
    draw(); replan();
  }

  // ---------------------------------------------------------- drawing
  function drawHead() {
    const n = base ? Object.keys(patchOf(base, draft())).length : 0;
    const st = kube?.status || {};
    const sub = managed()
      ? `StrataCluster ${kube.namespace}/${kube.name} · generation ${kube.generation ?? "–"} · the operator: ${st.phase || "not seen yet"}${st.message ? " · " + st.message : ""}`
      : "Machines of your own: the shape changes by admin calls, one at a time; settings are each server's flags";
    clear(head, h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, "Control center"), h("p", {}, sub)),
      h("div", { class: "row" },
        h("span", { class: "small " + (n ? "" : "muted"), style: n ? { color: "var(--accent)", fontWeight: 600 } : {} }, n ? `${plural(n, "change")} in the draft` : "the draft is the cluster as it is"),
        h("button", { class: "btn sm icon ctl-sq", title: "Undo (Ctrl+Z)", "aria-label": "Undo", disabled: pos === 0, onclick: undo }, icon("undo")),
        h("button", { class: "btn sm icon ctl-sq", title: "Redo (Ctrl+Shift+Z)", "aria-label": "Redo", disabled: pos >= hist.length - 1, onclick: redo }, icon("redo")),
        h("button", { class: "btn sm", disabled: !n, onclick: () => set((d) => { Object.keys(d).forEach((k) => delete d[k]); Object.assign(d, clone(base)); }) }, "Discard the draft"))));
  }
  const undo = () => { if (pos > 0) { pos--; changed(); } };
  const redo = () => { if (pos < hist.length - 1) { pos++; changed(); } };

  // The intent bar.
  const say = h("input", { class: "ctl-say", placeholder: "Say what the cluster should do: “add 2 shards”, “survive losing a server”, “double read capacity”, “hedge after 50 ms”", "aria-label": "Say what the cluster should do" });
  const understood = h("div", { class: "small" });
  const chips = h("div", { class: "row", style: { gap: "6px" } });
  let heard = null;
  function hear() {
    heard = draft() ? intent(say.value, draft(), L()) : null;
    clear(understood, !say.value.trim() ? null : heard ? h("span", { style: { color: "var(--accent)" } }, "Understood: " + heard.text + ". Enter adds it to the draft.") : h("span", { class: "muted" }, "Not sure what that asks for. Try one of these:"));
  }
  say.addEventListener("input", hear);
  say.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && heard) { e.preventDefault(); const it = heard; set(it.apply); toast("Added to the draft: " + it.text, "ok"); say.value = ""; hear(); }
    if (e.key === "Escape") { say.value = ""; hear(); }
  });
  function drawChips() {
    const d = draft(); if (!d) return;
    const list = managed()
      ? [`add ${d.shards >= 4 ? 2 : 1} shard${d.shards >= 4 ? "s" : ""}`, d.replicas < 3 ? "survive losing a server" : `${d.replicas + 1} copies`, "double read capacity", `grow volumes to ${(quantity(d.storage?.size) || 10 * 2 ** 30) * 2 / 2 ** 30}Gi`, "hedge after 50 ms", "log queries slower than 200 ms", "2 coordinators"]
      : [`add ${d.shards >= 4 ? 2 : 1} shard${d.shards >= 4 ? "s" : ""}`, d.replicas < 3 ? "3 copies" : `${d.replicas + 1} copies`, "remove 1 shard"];
    clear(chips, list.map((c) => h("button", { class: "ctl-chip", onclick: () => { const it = intent(c, draft(), L()); if (it) { set(it.apply); toast("Added to the draft: " + it.text, "ok"); } } }, c)));
  }
  clear(intentBox, h("div", { class: "ctl-intent" }, h("div", { class: "row nw", style: { gap: "10px" } }, icon("sparkle"), say), understood, chips));

  // The three columns: shape and guardrails; settings; what it does.
  const shapeBox = h("div"), guardBox = h("div"), settingsBox = h("div"), impactBox = h("div"), diffBox = h("div"), planBox = h("div");
  clear(mainBox, h("div", { class: "ctl-main" },
    h("div", { class: "ctl-col ctl-pin" }, shapeBox, guardBox),
    h("div", { class: "ctl-col" }, settingsBox),
    h("div", { class: "ctl-col ctl-pin" }, planBox, impactBox, diffBox)));

  function draw() {
    if (!draft()) { clear(shapeBox, card({ body: h("div", { class: "muted" }, "Reading the cluster…") })); return; }
    drawHead(); drawChips(); drawShape(); drawGuard(); drawSettings(); drawImpact(); drawDiff(); drawPlan();
  }

  // ---------------------------------------------------------- shape
  let drag = null;
  function drawShape() {
    const l = L(), d = draft();
    const S = d.shards, R = d.replicas, cols = Math.max(S, l.S), rows = Math.max(R, l.R, 1);
    const w = cols <= 10 ? 26 : cols <= 24 ? 16 : cols <= 64 ? 9 : 5, gap = cols <= 64 ? 3 : 1, hgt = Math.max(w, 14);
    const grid = h("div", { class: "ctl-grid", style: { gridTemplateColumns: `repeat(${cols}, ${w}px)`, gap: gap + "px" } });
    for (let j = 0; j < cols; j++) {
      const sh = l.shards[j];
      const col = h("div", { class: "ctl-shard" + (j >= S ? " going" : sh ? "" : " new"), style: { gap: gap + "px" } },
        cols <= 24 ? h("span", { class: "ctl-lbl" }, String(j)) : null);
      for (let k = 0; k < rows; k++) {
        const c = sh?.copies[k];
        let cls = "ctl-cell", title;
        if (c) {
          cls += c.primary ? " primary" : " replica";
          if (c.down) cls += " down";
          if (j >= S || k >= R) { cls += " going"; title = `${short(c.url)} goes`; }
          else title = `${short(c.url)}: ${c.primary ? "primary" : c.role || "replica"}${c.down ? ", down" : ""} of shard ${j}`;
        } else if (j < S && k < R) { cls += " new"; title = `a new copy of ${sh ? "shard " + j : "new shard " + j}`; }
        else cls += " none";
        col.append(h("span", { class: cls, title, style: { width: w + "px", height: hgt + "px" } }));
      }
      grid.append(col);
    }
    const handle = (kind) => {
      const btn = h("button", {
        class: "ctl-handle " + kind, type: "button",
        "aria-label": kind === "shards" ? `Shards: ${S}. Arrow keys change them.` : `Copies of each shard: ${R}. Arrow keys change them.`,
        title: kind === "shards" ? "Drag sideways, or use the arrow keys, to add or remove shards" : "Drag up or down, or use the arrow keys, to change the copies of each shard",
      }, icon("grip"));
      const stepPx = kind === "shards" ? w + gap : hgt + gap;
      // The drag follows the pointer on the window: the shape is drawn again
      // as the draft changes, handle and all.
      btn.addEventListener("pointerdown", (e) => { e.preventDefault(); drag = { kind, x: e.clientX, y: e.clientY, from: kind === "shards" ? S : R, stepPx, pending: false }; shapeBox.classList.add("dragging"); });
      btn.addEventListener("keydown", (e) => {
        // Right and up add a shard; down adds a copy (the copies grow downwards).
        const dd = kind === "shards" ? { ArrowRight: 1, ArrowUp: 1, ArrowLeft: -1, ArrowDown: -1, PageUp: 4, PageDown: -4 }[e.key] : { ArrowDown: 1, ArrowRight: 1, ArrowUp: -1, ArrowLeft: -1 }[e.key];
        if (dd == null) return;
        e.preventDefault();
        const f = kind === "shards" ? "shards" : "replicas", cap = kind === "shards" ? 256 : 9;
        set((x) => { x[f] = Math.max(1, Math.min(cap, x[f] + dd)); }, () => shapeBox.querySelector(`.ctl-handle.${kind}`)?.focus());
      });
      return btn;
    };
    const A = after(l, S, R);
    const stat = (label, now, then) => h("div", { class: "ctl-stat" }, h("span", { class: "small muted" }, label), h("b", {}, then), now !== then ? h("span", { class: "small muted" }, "now " + now) : null);
    const coord = managed() ? h("div", { class: "row between", style: { marginTop: "4px" } },
      h("span", { class: "small dim" }, "Coordinators behind the Service"),
      h("div", { class: "row nw", style: { gap: "6px" } },
        h("button", { class: "btn sm icon ctl-sq", "aria-label": "One coordinator fewer", disabled: d.coordinators <= 1, onclick: () => set((x) => { x.coordinators = Math.max(1, x.coordinators - 1); }) }, "−"),
        h("b", { style: { minWidth: "18px", textAlign: "center" } }, String(d.coordinators)),
        h("button", { class: "btn sm icon ctl-sq", "aria-label": "One more coordinator", onclick: () => set((x) => { x.coordinators += 1; }) }, "+"))) : null;
    clear(shapeBox, card({
      title: "Shape", sub: "drag the handles, or use the arrow keys on them",
      body: h("div", { class: "stack", style: { gap: "12px" } },
        h("div", { class: "ctl-shape" }, h("div", { class: "row nw", style: { alignItems: "stretch", gap: "6px" } }, grid, handle("shards")), handle("copies")),
        h("div", { class: "ctl-legend" },
          h("span", {}, h("i", { class: "ctl-cell primary" }), "primary"), h("span", {}, h("i", { class: "ctl-cell replica" }), "replica"),
          h("span", {}, h("i", { class: "ctl-cell new" }), "new"), h("span", {}, h("i", { class: "ctl-cell replica going" }), "goes"),
          l.down ? h("span", {}, h("i", { class: "ctl-cell replica down" }), "down") : null),
        h("div", { class: "ctl-stats" },
          stat("Shards × copies", `${l.S} × ${l.R}`, `${S} × ${R}`),
          stat("Servers", fmt.n(l.servers), fmt.n(S * R)),
          stat("Slots a shard", fmt.n(l.S ? Math.round(SLOTS / l.S) : 0), fmt.n(Math.round(SLOTS / S))),
          stat("Rows a shard", fmt.si(l.S ? l.rows / l.S : 0), fmt.si(A.rows))),
        coord),
    }));
  }

  function onDragMove(e) {
    if (!drag) return;
    const kind = drag.kind, f = kind === "shards" ? "shards" : "replicas";
    const delta = Math.round((kind === "shards" ? e.clientX - drag.x : e.clientY - drag.y) / drag.stepPx);
    const v = Math.max(1, Math.min(kind === "shards" ? 256 : 9, drag.from + delta));
    if (v === draft()[f]) return;
    // One undo step for the whole drag.
    if (!drag.pending) { drag.pending = true; set((x) => { x[f] = v; }); }
    else { hist[pos][f] = v; changed(); }
  }
  function onDragEnd() {
    if (!drag) return;
    const kind = drag.kind;
    drag = null;
    shapeBox.classList.remove("dragging");
    shapeBox.querySelector(`.ctl-handle.${kind}`)?.focus();
  }
  addEventListener("pointermove", onDragMove);
  addEventListener("pointerup", onDragEnd);
  addEventListener("pointercancel", onDragEnd);

  // ---------------------------------------------------------- guardrails
  function drawGuard() {
    if (!draft()) return;
    const x = ctx(), list = [];
    if (plan?.refused) list.push(["crit", plan.refused]);
    if (planErr) list.push(["crit", planErr]);
    for (const w of plan?.warnings || []) list.push(["warn", w]);
    if (managed()) for (const st of SETTINGS) { const w = st.warn?.(value(draft(), st), x); if (w && !list.some((l) => l[1].startsWith(w.slice(0, 18)))) list.push(["warn", w]); }
    if (x.L.down) list.push(["warn", `${plural(x.L.down, "copy is", "copies are")} down now: the operator waits for ${x.L.down === 1 ? "it" : "them"} before it restarts any`]);
    clear(guardBox, card({
      title: "Guardrails", sub: "what this draft makes less safe",
      body: list.length ? h("div", { class: "stack", style: { gap: "8px" } }, list.map(([sev, t]) => h("div", { class: "row nw ctl-warn " + sev, style: { alignItems: "flex-start" } }, h("span", { class: "fleet-sev " + (sev === "crit" ? "crit" : "warn") }), h("span", {}, t[0].toUpperCase() + t.slice(1) + ".")))) : h("div", { class: "small muted" }, "Nothing this draft makes less safe."),
    }));
  }

  const ctx = () => { const l = L(), d = draft(); return { L: l, d, S: d.shards, R: d.replicas, A: after(l, d.shards, d.replicas) }; };

  // ---------------------------------------------------------- settings
  function drawSettings() {
    if (!managed()) {
      clear(settingsBox, card({
        title: "Settings", sub: "each server's flags",
        body: h("div", { class: "stack", style: { gap: "10px" } },
          h("div", { class: "small dim" }, "On machines of your own each server keeps the flags it was started with. Change them where you start it, and restart one copy at a time: replicas first, then each primary after handing it over (Cluster, Make primary)."),
          h("div", { class: "small dim" }, `The coordinator hedges after ${state.cluster?.hedge_ms ?? "–"} ms (--hedge-ms).`),
          h("div", { class: "small dim" }, "On Kubernetes, the operator keeps every setting in the StrataCluster's spec and rolls each change out for you: see Cluster for how to run one.")),
      }));
      return;
    }
    const x = ctx(), d = draft();
    const groups = [...new Set(SETTINGS.map((s) => s.g))];
    const q = filter.trim().toLowerCase();
    const body = h("div", { class: "stack", style: { gap: "14px" } });
    let shownRows = 0;
    for (const g of groups) {
      const rows = SETTINGS.filter((s) => s.g === g).filter((st) => {
        const changedNow = !same(value(d, st), value(base, st));
        if (onlyChanged && !changedNow) return false;
        return !q || (st.label + " " + st.at + " " + g).toLowerCase().includes(q);
      });
      if (!rows.length) continue;
      shownRows += rows.length;
      body.append(h("div", { class: "ctl-group" }, h("div", { class: "ctl-gtitle" }, g), rows.map((st) => settingRow(st, d, x))));
    }
    if (!shownRows) body.append(h("div", { class: "small muted" }, onlyChanged ? "Nothing is changed yet." : "No setting matches."));
    const find = h("input", { class: "input sm", placeholder: "Find a setting", value: filter, style: { flex: "1", minWidth: "0" }, oninput: (e) => { filter = e.target.value; drawSettings(); settingsBox.querySelector("input.input.sm")?.focus(); } });
    body.prepend(h("div", { class: "row nw", style: { gap: "8px" } }, find,
      h("button", { class: "btn sm" + (onlyChanged ? " primary" : ""), "aria-pressed": String(onlyChanged), onclick: () => { onlyChanged = !onlyChanged; drawSettings(); } }, "Only changed")));
    clear(settingsBox, card({ title: "Settings", sub: "and where each value comes from", body, cls: "ctl-settings" }));
    if (q) { const f = settingsBox.querySelector("input.input.sm"); f.focus(); f.setSelectionRange(f.value.length, f.value.length); }
  }

  function settingRow(st, d, x) {
    const v = value(d, st), was = value(base, st), changedNow = !same(v, was);
    const [where, rest] = st.at.split(/:(.*)/s);
    const set1 = (nv) => set((dd) => assign(dd, st, nv));
    let control;
    if (st.type === "seg") control = seg(st.opts.map(([k, l]) => [k === undefined ? "" : k, l]), v ?? "", (nv) => set1(nv === "" ? undefined : nv));
    else if (st.type === "toggle") control = toggle(!!v, (on) => set1(on));
    else if (st.type === "step") {
      const list = st.list, cur = v ?? st.def;
      const lo = st.grows ? list.findIndex((s) => quantity(s) >= quantity(was)) : 0;
      const i = list.findIndex((s) => same(s, cur));
      control = h("div", { class: "row nw ctl-step" },
        h("button", { class: "btn sm icon ctl-sq", "aria-label": "Less " + st.label.toLowerCase(), disabled: i <= lo && i >= 0, onclick: () => set1(steps(list.slice(Math.max(0, lo)), cur, -1)) }, "−"),
        h("b", {}, shown(st, v)),
        h("button", { class: "btn sm icon ctl-sq", "aria-label": "More " + st.label.toLowerCase(), disabled: i === list.length - 1, onclick: () => set1(steps(list, cur, 1)) }, "+"));
    } else if (st.type === "text") {
      control = h("input", { class: "input sm ctl-text", value: v ?? "", placeholder: st.placeholder, onchange: (e) => set1(e.target.value.trim() || undefined) });
    } else if (st.type === "argv") {
      control = h("input", { class: "input sm ctl-text wide", value: line(v), placeholder: st.placeholder, onchange: (e) => set1(words(e.target.value)) });
    }
    const inSpec = where === "spec" ? getPath(kube.spec || {}, rest) !== undefined : where === "argv" ? (kube.spec?.[rest] || []).length > 0 : argOf(base[where === "args" ? "args" : "coordinatorArgs"], rest, st.bool) !== undefined;
    const prov = changedNow ? `was ${shown(st, was) ?? "unset"}` : where === "spec" ? (inSpec ? "in the spec" : "the operator's default") : where === "argv" ? (inSpec ? "in the spec" : "none") : inSpec ? `in ${where === "args" ? "args" : "coordinatorArgs"}` : "strata-server's default";
    const path = where === "spec" ? rest : where === "argv" ? rest : `${where === "args" ? "args" : "coordinatorArgs"}: ${rest}`;
    const effect = st.effect?.(v ?? st.def, x);
    return h("div", { class: "ctl-set" + (changedNow ? " changed" : "") + (st.type === "seg" || st.type === "argv" ? " stacked" : "") },
      h("div", { class: "ctl-set-l" },
        h("div", { class: "row nw", style: { gap: "6px" } }, h("b", {}, st.label), h("code", { class: "ctl-path" }, path)),
        effect ? h("div", { class: "small dim" }, effect) : null),
      h("div", { class: "ctl-set-r" }, control,
        h("div", { class: "row nw", style: { gap: "4px", justifyContent: "flex-end" } },
          h("span", { class: "small " + (changedNow ? "" : "muted"), style: changedNow ? { color: "var(--accent)" } : {} }, prov),
          changedNow ? h("span", { class: "small muted" }, "· " + (TAKES[st.takes] || "")) : null,
          changedNow ? h("button", { class: "btn sm icon ghost", title: "Put it back", "aria-label": "Put " + st.label.toLowerCase() + " back", onclick: () => set((dd) => assign(dd, st, was)) }, icon("reset")) : null)));
  }

  // ---------------------------------------------------------- impact
  function drawImpact() {
    const x = ctx(), l = x.L, A = x.A, d = draft();
    const row = (label, now, then, better) => h("div", { class: "ctl-imp" + (now !== then ? " moved" : "") }, h("span", { class: "small dim" }, label),
      h("span", { class: "small muted" }, now), h("b", { class: better == null || now === then ? "" : better ? "good" : "bad" }, then));
    const reqCpu = cores(getPath(d, "resources.requests.cpu"));
    const vol = quantity(d.storage?.size), volNow = quantity(base?.storage?.size);
    const usedNow = l.file ?? l.diskUsed, usedThen = A.file ?? A.diskUsed;
    const diskTotalNow = managed() && volNow ? volNow : l.diskTotal;
    const diskTotalThen = managed() && vol ? vol : l.diskTotal;
    const writes = (R, acks) => R >= 3 || (R === 2 && acks === "primary");
    const acks = managed() ? d.writeAcks : l.acks || "all";
    const surv = (label, ok, how) => h("div", { class: "row nw ctl-surv", style: { gap: "8px", alignItems: "flex-start" } }, h("span", { class: ok ? "ok" : "no" }, ok ? "✓" : "✕"), h("div", {}, h("span", {}, label), how ? h("div", { class: "small muted" }, how) : null));
    const fo = d.failoverAfterMs;
    clear(impactBox, card({
      title: "If you apply this", sub: "from the last minute's load",
      body: h("div", { class: "stack", style: { gap: "10px" } },
        h("div", { class: "ctl-imps" },
          h("div", { class: "ctl-imp hd" }, h("span"), h("span", { class: "small muted" }, "now"), h("span", { class: "small muted" }, "after")),
          row("Searches a copy serves", fmt.rate(l.R ? l.qps / l.R : 0), fmt.rate(A.qps), A.qps < l.qps / Math.max(1, l.R) ? true : A.qps > l.qps / Math.max(1, l.R) ? false : null),
          row("Cores a copy uses", l.cpu != null ? fmt.si(l.cpu) : "–", A.cpu != null ? fmt.si(A.cpu) + (reqCpu ? ` of ${fmt.si(reqCpu)}` : "") : "–", A.cpu != null && l.cpu != null ? A.cpu < l.cpu - 0.005 ? true : A.cpu > l.cpu + 0.005 ? false : null : null),
          row("Rows a shard holds", fmt.si(l.S ? l.rows / l.S : 0), fmt.si(A.rows), A.rows < l.rows / Math.max(1, l.S) ? true : A.rows > l.rows / Math.max(1, l.S) ? false : null),
          row(managed() ? "A copy's data, of its volume" : "A copy's data, of its disk", usedNow != null ? `${fmt.bytes(usedNow)}${diskTotalNow ? " · " + pct(usedNow / diskTotalNow) : ""}` : "–", usedThen != null ? `${fmt.bytes(usedThen)}${diskTotalThen ? " · " + pct(usedThen / diskTotalThen) : ""}` : "–",
            usedThen != null && diskTotalThen && diskTotalNow ? usedThen / diskTotalThen < usedNow / diskTotalNow - 0.005 ? true : usedThen / diskTotalThen > usedNow / diskTotalNow + 0.005 ? false : null : null),
          row("Servers", fmt.n(l.servers), fmt.n(d.shards * d.replicas), null)),
        h("div", { class: "small muted" }, `The cluster answers ${fmt.rate(l.qps)} searches now; each asks every shard once, and one copy of each answers.`),
        h("div", { class: "ctl-gtitle", style: { marginTop: "4px" } }, "Keeps writing when"),
        surv("a replica goes", writes(d.replicas, acks), d.replicas === 1 ? "there is none" : writes(d.replicas, acks) ? "the others hold every write" : `two copies, writes acknowledged by ${acks === "all" ? "every copy" : "a majority"}: a write waits for both`),
        managed()
          ? surv("a primary goes", fo > 0 && writes(d.replicas, acks), fo > 0 ? (d.replicas > 1 ? `a replica takes over after about ${sec(fo)}` : "with one copy there is no replica to take over") : "failover by hand: until someone promotes a copy")
          : surv("a primary goes", d.replicas > 1 && writes(d.replicas, acks), d.replicas > 1 ? "if the servers were started with --failover-after-ms; otherwise until someone promotes a copy" : "with one copy there is no replica to take over"),
        surv("a coordinator goes", managed() ? d.coordinators > 1 : false, managed() ? (d.coordinators > 1 ? `${d.coordinators - 1} more behind the Service` : "one coordinator: nothing answers until it is back") : "one coordinator here")),
    }));
  }

  // ---------------------------------------------------------- the spec diff
  function drawDiff() {
    if (!managed()) { clear(diffBox); return; }
    const lines = lineDiff(yaml(base), yaml(draft()));
    const changedAny = lines.some(([m]) => m !== " ");
    // Only the changes and two lines around each.
    const keep = lines.map((_, i) => lines.slice(Math.max(0, i - 2), i + 3).some(([m]) => m !== " "));
    const out = [];
    let skipped = 0;
    lines.forEach(([m, t], i) => {
      if (!keep[i] && changedAny) { skipped++; return; }
      if (skipped) { out.push(h("div", { class: "ctl-dl skip" }, `  … ${skipped} more`)); skipped = 0; }
      out.push(h("div", { class: "ctl-dl " + (m === "+" ? "add" : m === "-" ? "del" : "") }, (m === " " ? "  " : m + " ") + t));
    });
    if (skipped) out.push(h("div", { class: "ctl-dl skip" }, `  … ${skipped} more`));
    clear(diffBox, card({ title: "The change", sub: changedAny ? "to the StrataCluster's spec" : "the StrataCluster's spec", body: h("pre", { class: "ctl-diff" }, out) }));
  }

  // ---------------------------------------------------------- the plan
  function drawPlan() {
    if (!draft()) return;
    if (!managed()) return drawOwnPlan();
    const n = Object.keys(patchOf(base, draft())).length;
    const st = kube?.status || {};
    const progress = watching || said.length ? h("div", { class: "stack", style: { gap: "6px", marginTop: "10px" } },
      h("div", { class: "ctl-gtitle" }, watching ? "The operator is on it" : "The operator made the change"),
      h("ol", { class: "ctl-steps done" }, said.map((m, i) => h("li", { class: i === said.length - 1 && watching ? "now" : "" }, m))),
      !said.length ? h("div", { class: "small muted" }, "Waiting for the operator to read the new spec…") : null) : null;
    let body;
    if (!n) body = h("div", { class: "small muted" }, "Change something on the left, or say what you want in the bar above.");
    else if (planning && !plan) body = h("div", { class: "small muted" }, "Asking the coordinator…");
    else if (planErr) body = h("div", { class: "error" }, planErr);
    else if (plan) body = h("div", { class: "stack", style: { gap: "8px" } },
      h("ol", { class: "ctl-steps" }, (plan.steps || []).map((s) => h("li", {}, s))),
      plan.refused ? h("div", { class: "error" }, "Refused: " + plan.refused) : null);
    clear(planBox, card({
      title: "Plan", sub: n ? "the operator's steps, checked by the coordinator" : `the operator: ${st.phase || "–"}`,
      body: h("div", {}, body, progress),
      foot: h("div", { class: "row" },
        h("button", { class: "btn primary", disabled: !n || !plan || !!plan.refused || planning || applying, onclick: apply }, applying ? "Applying…" : n ? `Apply ${plural(plan?.changes?.length || n, "change")}` : "Apply"),
        h("span", { class: "small muted" }, "The draft stays in this browser until you apply or discard it.")),
    }));
  }

  function drawOwnPlan() {
    const p = run ? null : plan || ownPlan();
    const steps = run || p.steps;
    const needBox = p && p.need.length ? h("div", { class: "stack", style: { gap: "6px" } },
      h("div", { class: "small dim" }, `The gRPC URLs of ${plural(p.need.length, "new server")} (${p.need.join("; ")}), in that order. Start each with --seq-no and the cluster's schema (--schema), and --failover-after-ms like the others.`),
      h("textarea", { class: "input ctl-urls", rows: Math.min(6, p.need.length + 1), placeholder: "http://10.0.0.7:7701", value: urls, oninput: debounce((e) => { urls = e.target.value; plan = ownPlan(); drawOwnPlanOnly(); drawGuard(); }, 300) })) : null;
    const list = steps.length ? h("ol", { class: "ctl-steps" }, steps.map((s) => h("li", { class: s.st || "" },
      h("span", {}, s.st === "ok" ? "✓ " : s.st === "run" ? "… " : s.st === "bad" ? "✕ " : "", s.text), s.text2 && s.st === "run" ? h("span", { class: "small muted" }, " · " + s.text2) : null,
      s.err ? h("div", { class: "error", style: { marginTop: "4px" } }, s.err) : null,
      s.call && !run ? h("div", { class: "ctl-call" }, `${s.call[0]} ${s.call[1]}${s.call[2] && Object.keys(s.call[2]).length ? " " + JSON.stringify(s.call[2]) : ""}`) : null)))
      : h("div", { class: "small muted" }, "Drag the shape's handles, or say what you want in the bar above.");
    clear(planBox, card({
      title: "Plan", sub: "admin calls to this coordinator, one at a time",
      body: h("div", { class: "stack", style: { gap: "10px" } }, needBox, h("div", { class: "ctl-ownsteps" }, list), p?.refused ? h("div", { class: "small", style: { color: "var(--serious)" } }, p.refused[0].toUpperCase() + p.refused.slice(1) + ".") : null),
      foot: h("div", { class: "row" }, h("button", { class: "btn primary", disabled: !!run || !p || !!p.refused || !p.steps.length, onclick: runOwn }, run ? "Running…" : `Run ${plural(p?.steps.length || 0, "step")}`)),
    }));
  }
  // The steps only, so the URLs being typed keep their place.
  function drawOwnPlanOnly() {
    const box = planBox.querySelector(".ctl-ownsteps");
    if (!box) return drawOwnPlan();
    const p = plan;
    clear(box, p.steps.length ? h("ol", { class: "ctl-steps" }, p.steps.map((s) => h("li", {}, s.text, s.call ? h("div", { class: "ctl-call" }, `${s.call[0]} ${s.call[1]}${s.call[2] && Object.keys(s.call[2]).length ? " " + JSON.stringify(s.call[2]) : ""}`) : null))) : null);
    const btn = planBox.querySelector(".ft .btn.primary");
    if (btn) { btn.disabled = !!p.refused || !p.steps.length; btn.textContent = `Run ${plural(p.steps.length, "step")}`; }
    const why = planBox.querySelector(".bd > .stack > .small:last-child");
    if (why && why.style.color) why.textContent = p.refused ? p.refused[0].toUpperCase() + p.refused.slice(1) + "." : "";
  }

  // ---------------------------------------------------------- keys
  const onKey = (e) => {
    if (e.target.closest?.("input, textarea, [contenteditable]")) return;
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "z") { e.preventDefault(); if (e.shiftKey) redo(); else undo(); }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "y") { e.preventDefault(); redo(); }
  };
  addEventListener("keydown", onKey);

  drawHead(); draw();
  load();
  const offs = [
    bus.on("cluster", () => {
      if (!base && state.cluster && kube && !managed()) { const l = L(); reset({ shards: l.S, replicas: l.R, coordinators: 1 }); drawHead(); draw(); replan(); return; }
      if (!draft() || drag || document.activeElement?.closest?.(".ctl-main input, .ctl-main textarea")) return;
      drawShape(); drawImpact(); drawGuard();
      if (!managed() && !run) { plan = ownPlan(); drawOwnPlanOnly(); }
    }),
  ];
  const stopKube = every(10000, () => { if (!watching) api("/v1/kube").then((k) => { const was = kube; kube = k; if (managed() && !same(full(k.spec), base) && pos === 0) reset(full(k.spec)); if (!same(was?.status, k.status)) drawHead(); }).catch(() => {}); }, false);
  refreshCluster();
  return () => {
    offs.forEach((f) => f());
    removeEventListener("keydown", onKey);
    removeEventListener("pointermove", onDragMove);
    removeEventListener("pointerup", onDragEnd);
    removeEventListener("pointercancel", onDragEnd);
    stopWatch?.(); stopKube?.();
  };
}
