// The StrataCluster's spec as the command center edits it: the
// operator's defaults, every setting with where it lives and what it does,
// merge patches, and the spec as YAML with a line diff. Settings edits the
// settings, Rollouts shows the spec a version runs.

import { fmt } from "./core.js";

export const clone = (x) => JSON.parse(JSON.stringify(x ?? null));
export const same = (a, b) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
export const plural = (n, w, ws) => `${fmt.n(n)} ${n === 1 ? w : ws || w + "s"}`;
const short = (url) => (url || "").replace(/^https?:\/\//, "");

/** The spec's fields as the operator reads them when the spec leaves them out. */
export const DEFAULTS = {
  image: "chloedb-server:dev", imagePullPolicy: null, shards: 1, replicas: 3, coordinators: 1,
  storage: { size: "10Gi", storageClassName: null }, schema: null, demo: null, failoverAfterMs: 3000, writeAcks: "all",
  tokenSecret: null, clusterTokenSecret: null, resources: null, coordinatorResources: null, args: [], coordinatorArgs: [],
};

/** `raw` with every field the operator defaults filled in. */
export function full(raw) {
  const out = clone(DEFAULTS);
  for (const [k, v] of Object.entries(raw || {})) {
    if (k === "storage" && v && typeof v === "object") out.storage = { ...out.storage, ...v };
    else out[k] = clone(v);
  }
  return out;
}

/** The merge patch (RFC 7386) that takes `a` to `b`. */
export function patchOf(a, b) {
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
export function quantity(q) {
  const m = /^(\d+(?:\.\d+)?)(Ki|Mi|Gi|Ti|Pi|k|M|G|T|P|m)?$/.exec(String(q ?? "").trim());
  return m ? Number(m[1]) * UNITS[m[2] || ""] : null;
}
export const cores = (q) => (q == null ? null : quantity(q));

// ------------------------------------------------------------ args
/** Splits a command line into words, as a shell would (quotes, no expansion). */
export function words(text) {
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
export const line = (argv) => (argv || []).map(quote).join(" ");

/** The value of `--flag` in `argv`: its word, true for a switch, undefined if absent. */
export function argOf(argv, flag, bool) {
  for (let i = 0; i < (argv || []).length; i++) {
    const a = argv[i];
    if (a === flag) return bool ? true : argv[i + 1];
    if (a.startsWith(flag + "=")) return bool ? a.slice(flag.length + 1) !== "false" : a.slice(flag.length + 1);
  }
  return undefined;
}
export function withArg(argv, flag, v, bool) {
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
export const getPath = (o, path) => path.split(".").reduce((x, k) => (x == null ? undefined : x[k]), o);
export function setPath(o, path, v) {
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
export function live(c) {
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
export function after(L, S, R) {
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
export const steps = (list, v, d) => { let i = list.findIndex((x) => same(x, v)); if (i < 0) i = d > 0 ? -1 : list.length; return list[Math.max(0, Math.min(list.length - 1, i + d))]; };
export const sec = (ms) => (ms % 1000 ? (ms / 1000).toFixed(1) : ms / 1000) + " s";
const qty = (v, none) => (v == null ? none : String(v));
export const pct = (r) => (r > 0 && r < 0.001 ? "under 0.1%" : fmt.pct(r, r < 0.1 ? 1 : 0));
const every_ = (s) => (s < 60 ? s + " s" : s < 3600 ? s / 60 + " min" : s / 3600 + " h");

/**
 * Every setting the view edits: where it lives (`spec:path`, `args:--flag`
 * for every server, `cargs:--flag` for every coordinator), how it is
 * picked, what each value does, and what it takes to apply.
 */
export const SETTINGS = [
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
    // LIV-11: the request is what the scheduler keeps; only a limit restarts a server that uses more. A change here sets both.
    effect: (v, x) => {
      const limit = getPath(x.d || {}, "resources.limits.memory");
      const kept = !v ? "No request: the scheduler keeps no memory for the servers." : limit ? `Kept for each server${same(limit, v) ? ", and its limit" : `; its limit is ${limit}`}: one that uses more than ${limit} is restarted by Kubernetes.` : `Kept for each server by the scheduler. No limit is set, so one that uses more is not restarted, but may run its node short.`;
      return kept + (x.L.rss != null ? ` Each copy holds about ${fmt.bytes(x.L.rss)} now, for a database of ${fmt.bytes(x.L.file)}.` : "");
    },
    warn: (v, x) => {
      if (x.L.rss == null) return null;
      const limit = getPath(x.d || {}, "resources.limits.memory");
      if (limit && quantity(limit) < x.L.rss * 1.2) return `a copy holds about ${fmt.bytes(x.L.rss)} now: a limit of ${limit} would restart it`;
      if (v && quantity(v) < x.L.rss) return `a copy holds about ${fmt.bytes(x.L.rss)} now, more than the ${v} requested: the scheduler may put more on a node than it holds`;
      return null;
    }, takes: "servers" },
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
    effect: (v, x) => { const limit = getPath(x.d || {}, "coordinatorResources.limits.memory"); return !v ? "No request: the scheduler keeps no memory for the coordinators." : limit ? `Kept for each coordinator${same(limit, v) ? ", and its limit" : `; its limit is ${limit}`}: it merges the shards' answers.` : "Kept for each coordinator by the scheduler; no limit is set."; }, takes: "coordinators" },

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
    effect: () => "The arguments every server is started with, besides those the operator gives (chloedb-server --help lists them).", takes: "servers" },
  { g: "More arguments", label: "Every coordinator", at: "argv:coordinatorArgs", type: "argv", placeholder: "--flag value …",
    effect: () => "The arguments every coordinator is started with.", takes: "coordinators" },
];

export const TAKES = {
  servers: "restarts every server, one at a time",
  coordinators: "restarts the coordinators",
  volumes: "grows each volume in place",
  "new volumes": "applies to new volumes",
};

export function value(d, st) {
  const [where, rest] = st.at.split(/:(.*)/s);
  if (where === "spec") return getPath(d, rest) ?? undefined;
  if (where === "argv") return d[rest] || [];
  const v = argOf(d[where === "args" ? "args" : "coordinatorArgs"], rest, st.bool);
  if (st.bool) return !!v;
  if (v === undefined) return undefined;
  return typeof (st.list || []).find((x) => x != null) === "number" && !Number.isNaN(Number(v)) ? Number(v) : v;
}
export function assign(d, st, v) {
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
export const shown = (st, v) => (st.type === "seg" ? (st.opts.find((o) => same(o[0], v)) || [, String(v)])[1] : st.type === "toggle" ? (v ? "on" : "off") : st.show ? st.show(v ?? st.def) : v);

// ------------------------------------------------------------ YAML
export function yaml(spec) {
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
export function lineDiff(a, b) {
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

// ------------------------------------------------------------ a change of size, in words
/** 4 h 45 min, 22 min, 40 s. */
export function dur(secs) {
  if (secs == null || !Number.isFinite(secs)) return "–";
  secs = Math.max(0, Math.round(secs));
  if (secs < 60) return `${secs} s`;
  const m = Math.round(secs / 60);
  if (m < 60) return `${m} min`;
  const hr = Math.floor(m / 60), mm = m % 60;
  if (hr < 48) return mm ? `${hr} h ${mm} min` : `${hr} h`;
  return `${Math.floor(hr / 24)} d ${hr % 24} h`;
}
/** When a disk fills, from days: "about 7 h", "about 12 days". */
export function fullIn(days) {
  if (days == null || !Number.isFinite(days)) return null;
  if (days < 2) { const hr = Math.max(1, Math.round(days * 24)); return `about ${hr} h`; }
  return `about ${Math.round(days)} days`;
}
/** The rows a second the last slot move ran at, as the Cluster canvas kept it in this browser, or an assumed rate. */
export const ASSUMED_RATE = 20000;
export function keptRate(store) {
  const k = store?.get?.("cluster.rate", null);
  return k?.rate ? { rate: k.rate, measured: true, at: k.at } : { rate: ASSUMED_RATE, measured: false };
}
/**
 * What a StrataCluster patch of its size (shards, replicas, storage.size)
 * does, in words, from the cluster now (`S` shards of `R` copies holding
 * `rows`) and the spec (its servers' requests): the servers it adds or
 * removes, the room they need, the rows it copies and how long that takes
 * at `rate` rows a second (new copies of every shard at once; slots move
 * one job at a time). Never rounds a count: the numbers are the patch's.
 */
export function scaleWords(patch, { S, R, rows = 0, rate = ASSUMED_RATE, measured = false, spec = {}, slots = 4096 }) {
  const out = [];
  const R1 = patch.replicas ?? R, S1 = patch.shards ?? S;
  const per = rows / Math.max(1, S);
  const copiesOf = (n) => (n === 1 ? "one copy" : `${fmt.n(n)} copies`);
  let copyRows = 0, secs = 0, parallel = false;
  if (R1 !== R) {
    const d = R1 - R;
    out.push(d > 0
      ? `adds ${d === 1 ? "a copy" : fmt.n(d) + " copies"} to each of the ${fmt.n(S1)} shards: ${fmt.n(d * S1)} more servers (${fmt.n(S * R)} → ${fmt.n(S1 * R1)})`
      : `removes ${-d === 1 ? "a copy" : fmt.n(-d) + " copies"} from each of the ${fmt.n(S)} shards: ${fmt.n(-d * S)} servers fewer (${fmt.n(S * R)} → ${fmt.n(S1 * R1)}), and their volumes`);
    if (d > 0) { copyRows += d * S1 * per; secs += (d * per) / rate; parallel = true; }
  }
  if (S1 !== S) {
    const d = S1 - S;
    out.push(d > 0
      ? `adds ${plural(d, "shard")} of ${copiesOf(R1)}: ${fmt.n(d * R1)} more servers; about ${fmt.n(Math.round((slots * d) / S1))} slots move to them`
      : `removes ${plural(-d, "shard")} and their ${fmt.n(-d * R1)} servers; their slots move to the others first`);
    const moved = (rows * Math.abs(d)) / Math.max(S, S1);
    copyRows += moved; secs += moved / rate;
  }
  if (patch.storage?.size && patch.storage.size !== spec.storage?.size) out.push(`grows every volume to ${patch.storage.size}${spec.storage?.size ? ` (from ${spec.storage.size})` : ""}`);
  const added = S1 * R1 - S * R;
  const req = spec.resources?.requests || {};
  const vol = patch.storage?.size || spec.storage?.size || DEFAULTS.storage.size;
  const asks = [req.cpu ? `${fmt.n(added * (cores(req.cpu) || 0), 1)} cores` : null, req.memory ? `${fmt.bytes(added * (quantity(req.memory) || 0))} of memory` : null].filter(Boolean);
  const room = added > 0
    ? (asks.length ? `The new servers ask the nodes for ${asks.join(" and ")}, and ${fmt.n(added)} volumes of ${vol}.` : `The new servers need ${fmt.n(added)} volumes of ${vol}; the spec reserves them no cores or memory, so they take what their nodes have left.`)
    : null;
  const time = copyRows ? `About ${dur(secs)} to copy ${fmt.si(copyRows)} rows${parallel ? ", every shard at once" : ""}${measured ? ", at the rate the last move ran" : ", at an assumed rate (no move measured in this browser yet)"}.` : null;
  const text = out.length ? out.join("; ").replace(/^./, (c) => c.toUpperCase()) + "." : "Changes " + Object.keys(patch).join(", ") + ".";
  return { text, room, time, rows: copyRows, secs, added, removing: R1 < R || S1 < S, S1, R1 };
}
