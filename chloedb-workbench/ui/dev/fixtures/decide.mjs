// Decide's routes (SPEC.md, "Questions and rules"): three open questions
// (web's and refunds-api's from ask_human, each with what ChloeCtx found
// when it was asked, and lead's permission prompt), seven answered today by
// rules, one by you, and twelve rules. Answering one changes the rows the
// other screens read: the question, its outcome, its agent, and a new rule.
import { T, NOW, REPO, agents, byName } from "./base.mjs";
import { outcomes } from "./mission.mjs";

const hex8 = () => Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, "0");
const now = () => new Date().toISOString();

// An agent of an earlier session, as /api/ask maps a record's session to one.
const oldWeb = { ...byName.web, id: "a9e1d0c4", state: "exited", running: false, exit_code: 0, goal: null, outcome: null, created: new Date(NOW - 4 * 864e5).toISOString(), started_ms: NOW - 4 * 864e5, ended: new Date(NOW - 4 * 864e5 + 3600e3).toISOString() };
const day = (d, h, m) => { const x = new Date(NOW - d * 864e5); x.setHours(h, m, 0, 0); return x.toISOString(); };

export const rules = [
  ["Money is i64 cents, never floats.", 11, "q0a11e01"],
  ["Refunds are negative ledger entries, never a separate table.", 0.03, "q0a11e02"],
  ["Webhooks retry 5 times with exponential backoff, as charge.updated does.", 6, "q0a11e03"],
  ["Migrations run on the worktree's own database, never on the shared dev one.", 6, "q0a11e04"],
  ["The idempotency header is `Idempotency-Key`.", 5, "q0a11e05"],
  ["Every new API route gets an entry in docs/api/CHANGES.md.", 5, "q0a11e06"],
  ["Allow `cargo test` and `cargo sqlx prepare` without asking.", 4, "q0a11e07"],
  ["Error responses use the problem+json shape of src/api/errors.rs.", 3, "q0a11e08"],
  ["Dashboard tables page at 50 rows, server-side.", 3, "q0a11e09"],
  ["Never log a full card number; the last four digits at most.", 2, "q0a11e10"],
  ["Prefer extending charges' code over copying it for refunds.", 1, "q0a11e11"],
  ["Feature flags live in config/flags.toml, off by default.", 1, "q0a11e12"],
].map(([text, d, question], i) => ({
  id: `r00000${String(i + 1).padStart(3, "0")}`, repo: REPO, text, question, ctx_id: `e3b1c2d4:${100 + i * 7}`,
  active: i !== 10, created: d < 1 ? new Date(NOW - d * 864e5).toISOString() : day(d, 15, 20 + i),
}));

const q = (id, agent, o) => ({
  id, agent: byName[agent].id, outcome: byName[agent].outcome, repo: REPO, kind: "ask", context: null, recommended: null,
  precedent: { text: "", ids: [], agents: {}, records: [] }, state: "answered", answer: null, answered_by: null, rule: null,
  created: T(30), answered: T(30), ...o,
});
const PERM = [{ label: "Yes" }, { label: "Yes, and don't ask again" }, { label: "No" }];
// An open prompt's options as Workbench reads them off the agent's screen
// (SPEC.md, "Sixth build: Safe answering"): its own labels and numbers.
const PROMPT4 = [
  { label: "Yes", n: 1, no: false },
  { label: "Yes, and don't ask again for gh pr create commands in /work/shop", n: 2, no: false, grants: "stops it asking again for commands like this one" },
  { label: "Yes, and switch to auto mode", n: 3, no: false, grants: "switches the agent to auto mode: it stops asking for the rest of the session" },
  { label: "No", n: 4, no: true },
];
const PR_CMD = [
  "gh pr create --draft --title \"Refunds v2: ledger records and POST /refunds\" --body \"$(cat <<'BODY'",
  "## Outcomes",
  "- Ledger records refunds as negative entries",
  "- POST /refunds, idempotent on Idempotency-Key",
  "BODY",
  ")\"",
].join("\n");

export const questions = [
  q("q1c0ffee1", "web", {
    state: "open", answered: null, created: T(4),
    text: "Show partial refunds as their own rows, or nested under the charge?",
    context: "A charge of $120 refunded $20 then $30. The list today has one row per charge.",
    options: [
      { label: "Nested under the charge", detail: "The charge stays the unit; refunds show as indented lines with a running balance.", estimate: "web estimates 12 min · touches 2 files", matches_precedent: true },
      { label: "Their own rows", detail: "Every money movement is a row; easier to export, harder to read per customer.", estimate: "web estimates 20 min · touches 5 files, CSV export" },
    ],
    recommended: 0,
    suggested_rule: "In the dashboard, the charge is the unit; refunds and disputes nest under it.",
    precedent: {
      text: "Notes, newest last:\n- decision: Keep the charge as the unit. Disputes go under it.\n",
      ids: ["b4d08e51:3"], ms: 41, agents: { "b4d08e51:3": oldWeb },
      records: [
        { id: "b4d08e51:3", kind: "decision", text: "Keep the charge as the unit. Disputes go under it.", ts: day(4, 16, 12), by: "you", session: oldWeb.session_id, session_title: "dispute timeline", agent: oldWeb, score: 0.86,
          files: ["web/src/dashboard/DisputeRow.tsx", "web/src/dashboard/ChargeTimeline.tsx"], against: 0, rewind: { agent: oldWeb.id, from_ms: 812000, to_ms: 835000 } },
        { id: "b4d08e51:12", kind: "preference", text: "Money columns in the dashboard are right-aligned, with tabular figures.", ts: T(19), by: "web", agent: byName.web, score: 0.41, files: [] },
      ],
    },
  }),
  q("q2ab7e3c4", "refunds-api", {
    state: "open", answered: null, created: T(3),
    text: "Cap refunds at the original charge, or allow over-refunds for goodwill?",
    context: "A merchant asked to refund $130 of a $120 charge as a goodwill gesture. Today the API answers 422.",
    options: [
      { label: "Cap at the charge", detail: "Refunds of a charge total at most what was captured; goodwill goes out as a credit.", estimate: "refunds-api estimates 6 min · 1 file" },
      { label: "Allow over-refunds", detail: "Up to 10% over the charge, each one flagged for finance to review.", estimate: "refunds-api estimates 25 min · 4 files, a new limit setting" },
    ],
    recommended: 0,
    suggested_rule: "A charge's refunds never total more than what was captured; goodwill goes out as a credit.",
    precedent: {
      text: "Notes, newest last:\n- fact: The card networks reject a refund larger than the captured amount; the processor answers charge_exceeds_amount.\n",
      ids: ["5f1e9b07:44"], ms: 37, agents: {},
      records: [
        { id: "5f1e9b07:44", kind: "fact", text: "The card networks reject a refund larger than the captured amount; the processor answers `charge_exceeds_amount`.", ts: day(10, 11, 5), by: "a session", session_title: "processor sandbox", agent: null, score: 0.61, files: ["src/processor/errors.rs"] },
      ],
    },
  }),
  q("q3d9a0b17", "lead", {
    kind: "permission", state: "open", answered: null, created: T(1), outcome: null,
    text: "Claude needs your permission to use Bash",
    context: PR_CMD.split("\n")[0],
    options: PROMPT4,
    detail: {
      tool: "Bash", summary: "Bash: gh pr create --draft", command: PR_CMD, description: "Open a draft pull request for the two finished outcomes",
      menu: { kind: "permission", question: "Do you want to proceed?", header: "Bash command", footer: "Esc to cancel · Tab to amend", sig: "mock", labels: PROMPT4.map((o) => o.label) },
    },
  }),
  // Answered today by rules.
  q("q4e1c55a0", "hooks", { text: "Retry refund.updated 5 times or 8?", answer: "5 times, with exponential backoff", answered_by: "rule:r00000003", created: T(9), answered: T(9) }),
  q("q5b2f8d31", "ledger", { kind: "permission", text: "Run migration 0042 on the shared dev database?", context: "sqlx migrate run --database-url $DEV_DATABASE_URL", options: PERM, answer: "No", answered_by: "rule:r00000004", created: T(14), answered: T(14) }),
  q("q6a7d2e90", "refunds-api", { text: "Store refunds in their own table?", answer: "No: negative ledger entries", answered_by: "rule:r00000002", created: T(37), answered: T(37) }),
  q("q7c3b9f12", "ledger", { text: "Keep the refund ratio as an f64?", answer: "No: i64 cents", answered_by: "rule:r00000001", created: T(33), answered: T(33) }),
  q("q8e5a1c77", "refunds-api", { kind: "permission", text: "Claude needs your permission to use Bash", context: "cargo sqlx prepare --workspace", options: PERM, answer: "Yes", answered_by: "rule:r00000007", created: T(32), answered: T(32) }),
  q("q9f0d4b28", "refunds-api", { text: "Name the header Idempotency-Key or X-Request-Id?", answer: "Idempotency-Key", answered_by: "rule:r00000005", created: T(29), answered: T(29) }),
  q("qa1b6c3e5", "hooks", { text: "Log the card number in the webhook's debug output?", answer: "The last four digits at most", answered_by: "rule:r00000010", created: T(7), answered: T(7) }),
  // Answered by you, and in a terminal.
  q("qb8c2d0f4", "hooks", { text: "Emit refund.updated for partial refunds too?", answer: "Yes, one event per refund", answered_by: "you", created: T(22), answered: T(21) }),
  q("qc4d7e1a9", "ledger", { kind: "permission", text: "Claude needs your permission to use Bash", context: "cargo test refunds", options: PERM, answer: "Yes", answered_by: "terminal", created: T(12), answered: T(12) }),
];

// What each agent does once its question is answered.
const AFTER = {
  q1c0ffee1: ["Nesting refund lines under their charge, with a running balance.", "Their own rows: adding a refund row type and the CSV export."],
  q2ab7e3c4: ["Capping refunds at the captured amount; the 422 now says what is left.", "Allowing over-refunds up to 10%, flagged for finance."],
  q3d9a0b17: ["Opening a draft pull request for the two finished outcomes.", "Opening a draft pull request for the two finished outcomes.", "Holding the pull request; waiting on the other agents."],
};

function answer(qq, b) {
  const opts = Array.isArray(qq.options) ? qq.options : [];
  const n = b.option != null ? Number(b.option) : null;
  if (n != null && !opts[n]) return [400, { error: `no option ${n}`, error_code: "bad_request" }];
  if (n != null && b.label && b.label !== opts[n].label) return [409, { error: `Nothing was typed: option ${n + 1} of this prompt is “${opts[n].label}”, not “${b.label}”`, error_code: "not_typed" }];
  const text = n != null ? opts[n].label : b.cancel ? "Cancelled (Esc)" : String(b.text || "").trim();
  if (!text) return [400, { error: "an option or a text is needed", error_code: "bad_request" }];
  Object.assign(qq, { state: "answered", answer: text, answered_by: "you", answered: now() });
  let rule = null;
  if (b.rule && String(b.rule).trim()) {
    rule = { id: "r" + hex8(), repo: qq.repo, text: String(b.rule).trim(), question: qq.id, ctx_id: `e3b1c2d4:${300 + rules.length}`, active: true, created: now() };
    rules.unshift(rule);
    qq.rule = rule.id;
  }
  const a = agents.find((x) => x.id === qq.agent);
  if (a) {
    const said = (AFTER[qq.id] || [])[n ?? 0] || `Going on with your answer: ${text}`;
    Object.assign(a, { state: "working", state_since: now(), waiting_for: null, narration: said, narration_ts: now(), last_event: qq.kind === "permission" ? "Bash" : "ask_human answered" });
  }
  const o = outcomes.find((x) => x.id === qq.outcome);
  if (o && o.state === "needs_you" && !questions.some((x) => x.state === "open" && x.outcome === o.id)) Object.assign(o, { state: "active", progress: Math.min(95, o.progress + 8), updated: now() });
  return { question: qq, rule };
}

export const routes = [
  ["GET", /^\/api\/questions$/, (_, url) => {
    const repo = url.searchParams.get("repo"), state = url.searchParams.get("state");
    return { questions: questions.filter((x) => (!repo || x.repo === repo) && (!state || x.state === state)) };
  }],
  ["GET", /^\/api\/questions\/([^/]+)$/, (m) => {
    const x = questions.find((y) => y.id === m[1]);
    return x ? { question: x } : [404, { error: `no question ${m[1]}`, error_code: "not_found" }];
  }],
  ["POST", /^\/api\/questions\/([^/]+)\/answer$/, (m, _, b) => {
    const x = questions.find((y) => y.id === m[1]);
    if (!x) return [404, { error: `no question ${m[1]}`, error_code: "not_found" }];
    if (x.state !== "open") return [409, { error: `${x.id} was answered already`, error_code: "conflict" }];
    return answer(x, b || {});
  }],
  ["GET", /^\/api\/rules$/, (_, url) => {
    const repo = url.searchParams.get("repo");
    return { rules: rules.filter((r) => !repo || r.repo === repo) };
  }],
  ["PATCH", /^\/api\/rules\/([^/]+)$/, (m, _, b) => {
    const r = rules.find((x) => x.id === m[1]);
    if (!r) return [404, { error: `no rule ${m[1]}`, error_code: "not_found" }];
    if (b && typeof b.active === "boolean") r.active = b.active;
    return { rule: r };
  }],
];
