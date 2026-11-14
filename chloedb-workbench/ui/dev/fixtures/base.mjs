// The story every level fixture shares: a payments repo, "harbor", where
// you set the goal "Refunds v2" 46 minutes ago and a lead started four
// agents (the concept screens in /mnt/project-files/workbench/concepts/).
// Three questions are open (decide.mjs): web and refunds-api ask through
// ask_human, and lead waits on a permission prompt.
// Other fixture files import these and add their own routes.
export const NOW = Date.now();
export const T = (minAgo) => new Date(NOW - minAgo * 60000).toISOString();
export const ms = (minAgo) => NOW - minAgo * 60000;
export const REPO = "/home/user/src/harbor";
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/harbor-1a2b3c4d";

function agent(id, name, o) {
  return {
    id, name, repo: REPO, cwd: `${WT}/${name}`, branch: `wb/${name}`, parent: "a0000001", depth: 1,
    model: "sonnet", pid: 4000, exit_code: null, ended: null, running: true, children: [],
    session_id: `5e55${id.slice(1)}-0000-4000-8000-000000000000`, base: "9f1c2ab7e0d4", goal: "g7a3c1e09",
    ...o,
  };
}
export const agents = [
  agent("a0000001", "lead", { parent: null, depth: 0, task: "Refunds v2: partial refunds, idempotent, visible in the dashboard", state: "waiting", state_since: T(1), created: T(46), started_ms: ms(46), outcome: null, narration: "Wants to open a draft pull request for the two finished outcomes.", narration_ts: T(1), waiting_for: "Claude needs your permission to use Bash", children: ["a1ed6e12", "a2f4a0c3", "a3b7e5d1", "a4c9d2f0"], last_event: "Notification permission_prompt" }),
  agent("a1ed6e12", "ledger", { task: "Make refunds idempotent so a retried request never pays twice", state: "working", state_since: T(1), created: T(40), started_ms: ms(40), outcome: "o00000002", narration: "Fixing a race in the nightly batch: 2 of 3 tests pass.", narration_ts: T(0.3), last_event: "Bash cargo test refunds" }),
  agent("a2f4a0c3", "refunds-api", { task: "POST /refunds with validation and limits", state: "waiting", state_since: T(3), created: T(40), started_ms: ms(40), outcome: "o00000004", narration: "Cap refunds at the original charge, or allow over-refunds for goodwill?", narration_ts: T(3), waiting_for: "Cap refunds at the original charge?", last_event: "ask_human" }),
  agent("a3b7e5d1", "web", { task: "Show partial refunds in the dashboard", state: "waiting", state_since: T(4), created: T(38), started_ms: ms(38), outcome: "o00000003", narration: "Show partial refunds as their own rows, or nested under the charge?", narration_ts: T(4), waiting_for: "Show partial refunds as their own rows, or nested under the charge?", last_event: "ask_human" }),
  agent("a4c9d2f0", "hooks", { task: "Webhook refund.updated, signed and retried like charge.updated", state: "working", state_since: T(5), created: T(30), started_ms: ms(30), outcome: "o00000005", narration: "Reading how charge.updated is signed and retried.", narration_ts: T(0.8), last_event: "Read src/webhooks/sign.rs" }),
  agent("a5d3e8b1", "docs", { task: "Docs and changelog for refunds v2", state: "exited", state_since: T(50), created: T(70), started_ms: ms(70), outcome: null, narration: "Wrote the changelog draft.", narration_ts: T(50), exit_code: 0, ended: T(50), running: false, parent: null, depth: 0, goal: null }),
];
export const byName = Object.fromEntries(agents.map((a) => [a.name, a]));

export const routes = [
  ["GET", /^\/api\/agents$/, (_, url) => ({ agents: url.searchParams.get("all") ? agents : agents.filter((a) => a.running) })],
  ["GET", /^\/api\/agents\/([^/]+)$/, (m) => { const a = agents.find((x) => x.id === m[1]); return a ? { agent: a } : [404, { error: "no such agent", error_code: "not_found" }]; }],
  ["GET", /^\/api\/health$/, () => ({ ok: true, version: "0.1.0", agents_running: 4, max_agents: 8 })],
];
