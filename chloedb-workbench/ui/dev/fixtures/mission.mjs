// Mission's routes (SPEC.md, "Goals and outcomes"): the goal "Refunds v2" of
// the harbor repo, its seven outcomes, what the fleet learned today, the
// collision ahead and the last hour's pulse. Another repo (`?repo=` anything
// else in the harness) has no goal, so the "Set a goal" box shows; posting
// one there makes it, with a lead that is still planning.
import { T, ms, NOW, REPO, agents, byName } from "./base.mjs";
import { questions } from "./decide.mjs";

const hex8 = () => Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, "0");

export const goals = [
  { id: "g7a3c1e09", repo: REPO, text: "Refunds v2: partial, idempotent, visible in the dashboard", state: "active", lead: "a0000001", created: T(46), done: null },
];

const oc = (n, title, detail, state, agent, progress, o = {}) => ({
  id: `o0000000${n}`, goal: "g7a3c1e09", title, detail, state, agent: agent ? byName[agent].id : null, ord: n, after: null, progress,
  created: T(45), updated: T(2), ...o,
});
export const outcomes = [
  oc(1, "Refund records in the ledger", "Posted refunds as negative entries, same table as charges.", "done", "ledger", 100, { updated: T(31) }),
  oc(2, "Idempotent refunds", "A retried request with the same (merchant, client_ref) never pays twice.", "active", "ledger", 72),
  oc(3, "Partial refunds in the dashboard", "Each refund under its charge, with what is left to refund.", "needs_you", "web", 45),
  oc(4, "Refund limits per merchant", "Validation and per-merchant caps for POST /refunds.", "needs_you", "refunds-api", 60),
  oc(5, "Webhook: refund.updated", "Signed and retried like charge.updated.", "active", "hooks", 18),
  oc(6, "API: POST /refunds", "Endpoint, validation, 14 tests green.", "done", "refunds-api", 100, { updated: T(24) }),
  oc(7, "Docs and changelog", "Starts when the API and the webhook land.", "next", null, 0, { after: "o00000005" }),
];

const rec = (id, kind, text, minAgo, agent, o = {}) => ({ id, kind, text, ts: T(minAgo), session: agent ? agent.session_id : null, agent, ...o });
const learned = [
  rec("e3b1c2d4:107", "decision", "Refunds are negative ledger entries, never a separate table.", 42, null, { by: "you" }),
  rec("9a71f0e2:31", "decision", "Idempotency key is (merchant, client_ref), as for charges.", 35, byName.ledger),
  rec("e3b1c2d4:140", "fix", "“sqlx offline data missing” → run `cargo sqlx prepare`.", 33, byName["refunds-api"]),
  rec("b4d08e51:12", "preference", "Money columns in the dashboard are right-aligned, with tabular figures.", 19, byName.web),
  rec("c7e2a9f3:8", "fact", "charge.updated is signed with HMAC-SHA256 over the raw body, one key per merchant.", 6, byName.hooks),
];

const heads_up = [
  { path: "src/ledger/post.rs", agents: [byName.ledger.id, byName.hooks.id], kind: "heading_into", detail: "ledger changed it 4 min ago; hooks read it 2 min ago and is about to emit refund.updated from it." },
];

// Events per minute for the last hour: busy while four agents work, quieter
// when they wait.
const pulse = Array.from({ length: 60 }, (_, i) => {
  const m = 59 - i;
  const base = m > 46 ? 2 : m > 40 ? 9 : 14;
  const wave = Math.round(6 * Math.sin(i * 0.7) + 4 * Math.cos(i * 1.9));
  return [Math.floor((NOW - m * 60000) / 60000) * 60000, Math.max(0, base + wave + (m < 4 ? -6 : 0))];
});

function detail(g) {
  const os = outcomes.filter((o) => o.goal === g.id);
  const ids = new Set([g.lead, ...os.map((o) => o.agent)].filter(Boolean));
  const ags = agents.filter((a) => ids.has(a.id) || a.goal === g.id);
  const sum = (k) => ags.reduce((n, a) => n + (a.tokens ? a.tokens[k] : 0), 0);
  const real = g.repo === REPO;
  return {
    goal: g, outcomes: os, agents: ags,
    open_questions: questions.filter((q) => q.state === "open" && q.repo === g.repo).length,
    learned: real ? learned : [], heads_up: real ? heads_up : [], pulse: real ? pulse : pulse.map(([t]) => [t, 0]),
    // Assumed beyond SPEC.md: the goal's tokens, summed from its agents' transcripts.
    tokens: real ? { input: 612400, output: 48300, cache_read: 2910000 } : { input: sum("input"), output: sum("output"), cache_read: 0 },
  };
}

export const routes = [
  ["GET", /^\/api\/goals$/, (_, url) => {
    const repo = url.searchParams.get("repo");
    return { goals: goals.filter((g) => !repo || g.repo === repo) };
  }],
  ["GET", /^\/api\/goals\/([^/]+)$/, (m) => {
    const g = goals.find((x) => x.id === m[1]);
    return g ? detail(g) : [404, { error: `no goal ${m[1]}`, error_code: "not_found" }];
  }],
  ["POST", /^\/api\/goals$/, (_, __, b) => {
    if (!b || !b.repo || !String(b.text || "").trim()) return [400, { error: "repo and text are needed", error_code: "bad_request" }];
    const g = { id: "g" + hex8(), repo: b.repo, text: String(b.text).trim(), state: "active", lead: null, created: new Date().toISOString(), done: null };
    if (b.plan) {
      const n = agents.filter((a) => a.name === "lead" || a.name.startsWith("lead-")).length;
      const lead = {
        ...byName.lead, id: "a" + hex8(), name: n ? `lead-${n + 1}` : "lead", repo: b.repo, cwd: b.repo, branch: "main", goal: g.id, children: [],
        task: g.text, state: "working", state_since: g.created, created: g.created, started_ms: Date.now(), waiting_for: null,
        narration: "Reading the repository to break the goal into outcomes.", narration_ts: g.created, last_event: "Read README.md",
      };
      agents.push(lead);
      g.lead = lead.id;
    }
    goals.unshift(g);
    return [201, { goal: g }];
  }],
  ["POST", /^\/api\/outcomes$/, (_, __, b) => {
    const o = { id: "o" + hex8(), goal: b.goal, title: b.title, detail: b.detail || null, state: "next", agent: null, ord: outcomes.length + 1, after: b.after || null, progress: 0, created: new Date().toISOString(), updated: new Date().toISOString() };
    outcomes.push(o);
    return [201, { outcome: o }];
  }],
  ["PATCH", /^\/api\/outcomes\/([^/]+)$/, (m, _, b) => {
    const o = outcomes.find((x) => x.id === m[1]);
    if (!o) return [404, { error: `no outcome ${m[1]}`, error_code: "not_found" }];
    for (const k of ["title", "state", "progress", "agent"]) if (b && b[k] !== undefined) o[k] = b[k];
    o.updated = new Date().toISOString();
    return { outcome: o };
  }],
  ["POST", /^\/api\/agents\/([^/]+)\/steer$/, (m, _, b) => {
    const a = agents.find((x) => x.id === m[1]);
    if (!a) return [404, { error: `no agent ${m[1]}`, error_code: "not_found" }];
    if (!String((b && b.text) || "").trim()) return [400, { error: "text is empty", error_code: "bad_request" }];
    const typed = a.state === "idle";
    return { ok: true, delivered: typed ? "typed" : "next_tool_call" };
  }],
];

