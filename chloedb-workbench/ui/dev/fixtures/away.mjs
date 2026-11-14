// GET /api/digest for the harbor story (SPEC.md, "Away"): you set the goal,
// left two minutes later, and come back now. POST /api/seen is recorded but
// the fixture keeps the away window fixed, so every reload tells the story.
import { REPO, NOW, T, ms, byName } from "./base.mjs";

const A = byName;
export const AWAY_FROM = ms(44);
export const seen = [];

export const questions = [
  { id: "q3c81f2a0", agent: A["refunds-api"].id, outcome: "o00000004", repo: REPO, kind: "ask",
    text: "Cap refunds at the original charge, or allow over-refunds for goodwill?",
    context: "POST /refunds checks the amount against what is left of the charge. Support asked for goodwill refunds above it. Both are built behind a flag, so either answer is a one-line change.",
    options: [
      { label: "Cap at the charge", detail: "Refunds never exceed what is left of the charge; goodwill goes through credits.", matches_precedent: true },
      { label: "Allow over-refunds", detail: "Up to 10% above the charge, logged with a reason." },
    ],
    recommended: 0,
    precedent: { records: [
      { id: "m3920", kind: "preference", text: "Refund limits come from the merchant's plan, never from a constant.", ts: T(60 * 24 * 6), agent: null },
      { id: "m2210", kind: "decision", text: "Never pay out more than was captured; goodwill is a credit, not a refund.", ts: T(60 * 24 * 19), agent: null },
    ] },
    state: "open", answer: null, answered_by: null, rule: null, created: T(3), answered: null },
  { id: "q1a7700b2", agent: A.web.id, outcome: "o00000003", repo: REPO, kind: "ask",
    text: "Show refunds as nested lines under the charge, or as their own table?", context: null,
    options: [{ label: "Nested under the charge" }, { label: "Their own table" }], recommended: 0, precedent: null,
    state: "answered", answer: "Nested under the charge", answered_by: "rule:r5b0e2c11", rule: null, created: T(25), answered: T(25) },
  { id: "q9e04d6c3", agent: A.hooks.id, outcome: "o00000005", repo: REPO, kind: "permission",
    text: "Allow Bash: cargo test -p harbor webhooks?", context: null,
    options: [{ label: "Yes" }, { label: "Yes, and don't ask again" }, { label: "No" }], recommended: null, precedent: null,
    state: "answered", answer: "Yes", answered_by: "rule:r77d1a0f4", rule: null, created: T(14), answered: T(14) },
  { id: "q5d2b9e14", agent: A.ledger.id, outcome: "o00000002", repo: REPO, kind: "ask",
    text: "Store refund amounts as i64 cents or as decimal?", context: null,
    options: [{ label: "i64 cents" }, { label: "decimal" }], recommended: 0, precedent: null,
    state: "answered", answer: "i64 cents", answered_by: "rule:r1f6a3d58", rule: null, created: T(29), answered: T(29) },
  { id: "q7b0c4a95", agent: A.hooks.id, outcome: "o00000005", repo: REPO, kind: "ask",
    text: "Retry refund.updated like charge.updated (5 attempts, 1/5/25 min)?", context: null,
    options: [{ label: "Same as charge.updated" }, { label: "Fewer retries" }], recommended: 0, precedent: null,
    state: "answered", answer: "Same as charge.updated", answered_by: "terminal", rule: null, created: T(18), answered: T(17) },
];

export const rules = [
  { id: "r5b0e2c11", repo: REPO, text: "Refund lines nest under their charge.", question: null, ctx_id: "m4120", active: true, created: T(60 * 24 * 4) },
  { id: "r77d1a0f4", repo: REPO, text: "Agents may run cargo test in this repo without asking.", question: null, ctx_id: "m4001", active: true, created: T(60 * 24 * 2) },
  { id: "r1f6a3d58", repo: REPO, text: "Money is i64 cents, never floats.", question: null, ctx_id: "m4411", active: true, created: T(60 * 24 * 11) },
  { id: "r0c4f9e27", repo: REPO, text: "Every money endpoint takes an idempotency key.", question: null, ctx_id: null, active: false, created: T(6),
    proposed: true, evidence: "seen in 3 agents' choices while you were away" },
];

const story = [
  { ts: T(41), agent: A.lead, text: "split **Refunds v2** into five outcomes and started ledger, refunds-api, web and hooks, one worktree each.", chips: [] },
  { ts: T(31), agent: A.ledger, text: "found the double-payout race in the nightly batch and reused the lock from `charges/batch.rs`.", chips: [{ kind: "deja_vu", text: "déjà vu: the 4 Nov fix" }] },
  { ts: T(25), agent: A.web, text: "nested refunds under their charge, **as you chose on 10 Nov**, without asking.", chips: [{ kind: "rule", text: "rule from 10 Nov" }] },
  { ts: T(17), agent: A.hooks, text: "waited for ledger instead of colliding in `post.rs`, then signed **refund.updated** exactly like charge.updated.", chips: [{ kind: "decision", text: "5 retries, from 6 Nov" }] },
  { ts: T(12), agent: A.web, text: "made the Refund button refund only what is left of the charge.", chips: [{ kind: "preference", text: "never more than the charge" }] },
  { ts: T(9), agent: A.ledger, text: "has **2 of 3 idempotency tests** passing; the third is the batch race it is fixing now.", chips: [{ kind: "test", text: "2 / 3 green" }] },
  { ts: T(3), agent: A["refunds-api"], text: "is **still waiting on you**: cap refunds at the charge, or allow over-refunds? It built both behind a flag.", chips: [{ kind: "waiting", text: "waiting 3 min" }] },
  { ts: T(2), agent: null, text: "ChloeCtx noticed three agents choosing idempotency keys on their own and proposes a rule.", chips: [{ kind: "rule", text: "proposed rule" }] },
];

// Rewind times are milliseconds since the agent's session started, as the
// recording route and the Why card's rewind count them.
const at = (a, from, to, label) => ({ agent: a.id, from_ms: Math.round(ms(from) - a.started_ms), to_ms: Math.round(ms(to) - a.started_ms), label });
const replay = [
  at(A.ledger, 31.6, 31, "$ cargo test refunds\ntest two_workers … FAILED"),
  at(A.ledger, 30.2, 29.6, "● Applying the lock\nfrom charges/batch.rs"),
  at(A.web, 25.3, 24.8, "RefundLines.tsx\n+64 −12 · rule from 10 Nov"),
  at(A.hooks, 17.4, 16.9, "⏸ hooks waits for\nledger (post.rs)"),
  at(A.hooks, 14.2, 13.8, "✓ cargo test webhooks\nallowed by your rule"),
  at(A["refunds-api"], 3.5, 3, "? Cap refunds at the\noriginal charge?"),
];

const agentStart = (id) => Object.values(A).find((a) => a.id === id)?.started_ms || 0;
function digest(since) {
  const open = questions.filter((q) => q.state === "open");
  const answered = questions.filter((q) => q.state === "answered" && Date.parse(q.answered) >= since);
  return {
    since_ms: since, until_ms: NOW,
    tiles: { done: 2, decided: answered.length, waiting: open.length, learned: 3 },
    story: story.filter((s) => Date.parse(s.ts) >= since),
    questions: [...open, ...answered],
    rules,
    replay: replay.filter((r) => agentStart(r.agent) + r.to_ms >= since),
  };
}

export const routes = [
  ["GET", /^\/api\/digest$/, (_, url) => digest(Number(url.searchParams.get("since")) || AWAY_FROM)],
  ["POST", /^\/api\/seen$/, () => { seen.push(Date.now()); return { ok: true, last_seen: new Date().toISOString() }; }],
  ["POST", /^\/api\/questions\/([^/]+)\/answer$/, (m, _, body) => {
    const q = questions.find((x) => x.id === m[1]);
    if (!q) return [404, { error: "no such question", error_code: "not_found" }];
    if (q.state !== "open") return [409, { error: "already answered", error_code: "conflict" }];
    const text = body?.text || (body?.option != null ? q.options[body.option]?.label : null);
    if (!text) return [400, { error: "an option or a text is required", error_code: "bad_request" }];
    Object.assign(q, { state: "answered", answer: text, answered_by: "you", answered: new Date().toISOString() });
    const a = Object.values(A).find((x) => x.id === q.agent);
    if (a && a.state === "waiting") Object.assign(a, { state: "working", waiting_for: null, state_since: new Date().toISOString() });
    if (body?.rule) {
      const r = { id: "r" + Math.random().toString(16).slice(2, 10), repo: REPO, text: body.rule, question: q.id, ctx_id: null, active: true, created: new Date().toISOString() };
      rules.push(r);
      q.rule = r.id;
    }
    return { question: q };
  }],
  ["PATCH", /^\/api\/rules\/([^/]+)$/, (m, _, body) => {
    const r = rules.find((x) => x.id === m[1]);
    if (!r) return [404, { error: "no such rule", error_code: "not_found" }];
    if (typeof body?.active === "boolean") { r.active = body.active; if (r.active) r.proposed = false; }
    return { rule: r };
  }],
];
