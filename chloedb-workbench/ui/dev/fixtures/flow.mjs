// GET /api/flow for the harbor story (SPEC.md, "Flow"): where each agent's
// time went, as segments from Workbench's events, with the coach's
// suggestions and `chloectx report --json --days 1` as it prints it.
import { NOW, ms, byName } from "./base.mjs";

// A seeded generator, so every load draws the same day.
function rng(seed) {
  return () => {
    seed |= 0; seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const AVG = { model: 70, tools: 48, tests: 105, waiting: 95, redo: 62, idle: 55 };
function lane(seed, from, to, weights, tail) {
  const r = rng(seed);
  const kinds = Object.keys(weights);
  const total = kinds.reduce((s, k) => s + weights[k], 0);
  const segs = [];
  let t = from;
  const end = tail ? tail[0] : to;
  let prev = null;
  while (t < end - 5000) {
    let x = r() * total, k = kinds[0];
    for (const c of kinds) { x -= weights[c]; if (x <= 0) { k = c; break; } }
    if (k === prev) continue;
    const d = Math.round(AVG[k] * (0.45 + r() * 1.25) * 1000);
    const e = Math.min(end, t + d);
    segs.push([t, e, k]);
    prev = k;
    t = e;
  }
  if (tail) segs.push([tail[0], to, tail[1]]);
  return segs;
}
const id = (n) => byName[n].id;
const LANES = {
  [id("lead")]: [[ms(46), ms(44.2), "model"], [ms(44.2), ms(41), "tools"], [ms(41), ms(40.2), "model"], [ms(40.2), ms(31), "tools"], [ms(31), ms(30), "model"], [ms(30), NOW, "idle"]],
  [id("ledger")]: lane(11, ms(40), NOW, { model: 26, tools: 18, tests: 26, waiting: 6, redo: 18, idle: 3 }, [ms(1.2), "tests"]),
  [id("refunds-api")]: lane(23, ms(40), NOW, { model: 30, tools: 21, tests: 14, waiting: 20, redo: 8, idle: 3 }, [ms(3), "waiting"]),
  [id("web")]: lane(37, ms(38), NOW, { model: 38, tools: 27, tests: 9, waiting: 14, redo: 6, idle: 3 }),
  [id("hooks")]: lane(41, ms(30), NOW, { model: 34, tools: 30, waiting: 26, redo: 6, idle: 2 }),
  [id("docs")]: lane(53, ms(70), ms(50), { model: 58, tools: 38, idle: 4 }),
};

const KINDS = ["model", "tools", "tests", "waiting", "redo", "idle"];
export const suggestions = [
  { id: "allow:cargo-test-harbor", title: "Allow `cargo test -p harbor` in this repo",
    detail: "You approved it 9 times in the last hour; each approval stopped an agent for 1.5 min on average.",
    saves: "~14 min/h", action: { kind: "allow", pattern: "Bash(cargo test -p harbor:*)" } },
  { id: "fix:sqlx-offline", title: "refunds-api hit “sqlx offline data missing” 3 times",
    detail: "Same error as on 4 Nov; the fix then was `cargo sqlx prepare`. Make it a fix the fleet gets on that error.",
    saves: "~6 min", action: { kind: "fix", error: "error: `SQLX_OFFLINE=true` but there is no cached data for this query" } },
  { id: "capsule:hooks", title: "hooks re-read 6 files 4 times",
    detail: "It starts cold each turn. Hand it ledger's capsule (its decisions and the files it touched) in its next prompt instead.",
    saves: "~9k tokens", action: { kind: "capsule" } },
  { id: "sequence:src/ledger", title: "Two agents in `src/ledger` collided twice",
    detail: "ledger and hooks both edit post.rs. Split the work by file, or let hooks start there once ledger is done.",
    saves: "1 merge fix", action: { kind: "sequence", path: "src/ledger", agents: [id("ledger"), id("hooks")] } },
];

const report = {
  project: "harbor", days: 1, since: Math.round(NOW / 1000) - 86400, until: Math.round(NOW / 1000), resolution: "seconds", wait_cap_secs: 1800,
  total: { model: 4210, tools: 3120, waiting: 1860, permission: 820, compaction: 140, away: 3600 },
  turns: 61, tool_calls: 512, timed_calls: 498, compactions: 3, compactions_per_hour: 1.2, permission_prompts: 14, subagents: 2,
  sessions: [], commands: [{ command: "cargo test -p harbor", runs: 14, timed: 14, total_secs: 1300, max_secs: 160, failed: 6 }],
  tools: [{ tool: "Bash", calls: 140, total_secs: 2400 }, { tool: "Edit", calls: 96, total_secs: 120 }, { tool: "Read", calls: 210, total_secs: 90 }],
  errors: [], rereads: [{ file: "src/ledger/post.rs", session: "5e55c9d2f0", reads: 6 }],
  suggestions: [
    { kind: "slow_command", text: "`cargo test -p harbor` ran 14 times and took 21m 40s in all (1m 32s a run, and failed 6 of those times). A narrower run while iterating (one test, one package, an incremental or cached build) would save most of that.", seconds: 1300 },
    { kind: "permission", text: "Claude asked for permission 14 times (Bash 11 times, Edit 3 times) and waited 13m 40s in all for the answers: allowing the ones you always approve in the project's `.claude/settings.json` (`permissions.allow`) would spare the waits.", seconds: 820 },
    { kind: "compaction", text: "The context was compacted 3 times, 1.2 an hour of work (2m 20s compacting), and each loses detail: a new session per task, and long outputs (logs, whole files) kept out of the conversation, make it rarer.", seconds: 1040 },
  ],
};

function flow(minutes) {
  const from = NOW - minutes * 60000, to = NOW;
  const totals = Object.fromEntries(KINDS.map((k) => [k + "_ms", 0]));
  const out = [];
  for (const [aid, segs] of Object.entries(LANES)) {
    const clipped = segs.filter(([s, e]) => e > from && s < to).map(([s, e, k]) => [Math.max(s, from), Math.min(e, to), k]);
    if (!clipped.length) continue;
    const row = { agent: Object.values(byName).find((a) => a.id === aid) };
    for (const k of KINDS) row[k + "_ms"] = 0;
    for (const [s, e, k] of clipped) { row[k + "_ms"] += e - s; totals[k + "_ms"] += e - s; }
    row.segments = clipped;
    out.push(row);
  }
  const f = minutes / 60;
  return {
    window: { from_ms: from, to_ms: to },
    totals,
    agents: out,
    tokens: { input: Math.round(412000 * f), output: Math.round(96400 * f), cache_read: Math.round(2310000 * f) },
    suggestions,
    report,
  };
}

export const routes = [
  ["GET", /^\/api\/flow$/, (_, url) => flow(Math.max(1, Number(url.searchParams.get("minutes") || 60)))],
  ["POST", /^\/api\/flow\/apply$/, (_, __, body) => {
    const s = suggestions.find((x) => x.id === body?.id);
    if (!s) return [404, { error: "no such suggestion", error_code: "not_found" }];
    if (!s.action || s.action.kind === "capsule") return [400, { error: "this suggestion has no action", error_code: "bad_request" }];
    if (s.action.kind === "fix" && !String(body.text || "").trim()) return [400, { error: "a fix needs its text", error_code: "bad_request" }];
    s.applied = true;
    return { ok: true, suggestion: s };
  }],
];
