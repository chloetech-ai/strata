// The harness mock's data: a week of the demo repository "shop" (Python:
// money.py, cart.py, coupons.py, serve.py, a checkout page in web/), as the
// rows HARNESS.md names would hold it. "Now" is 07:31 on 14 Nov 2026, the
// journeys mock's morning: cents and flakehunt are ready, paybutton waits on
// you, freeship works on the shipping line (it taxed shipping at turn 2).

/** The harness's versions, newest first (harness_versions). */
export const versions = [
  { v: "1.5-rc", sha: "c71e0a9", by: "optimizer", from: "run 7", when: "today 07:29", state: "candidate", note: "allow PYTHONPATH=. python3 -m pytest; pin your decisions to the files they name", sessions: "60 replays", success: "8/10", prompts: "0.4", graph: "○" },
  { v: "1.4", sha: "a3f9c21", by: "you", from: "Rules", when: "Nov 12", state: "current", note: "rule: Run the tests with python3 -m pytest -q from the repository root", sessions: "9", success: "5/8", prompts: "2.6", graph: "●" },
  { v: "1.4-hooks", sha: "b81d4e0", by: "you", from: "Editor", when: "Nov 13", state: "draft", note: "chloectx's UserPromptSubmit timeout 5 s → 3 s", sessions: "0", success: "–", prompts: "–", graph: "◌", branch: true },
  { v: "1.3", sha: "5d20e4b", by: "Flow", from: "Where the time went", when: "Nov 8", state: "", note: "allow Bash(python3 serve.py:*)", sessions: "3", success: "2/3", prompts: "3.3", graph: "●" },
  { v: "1.2", sha: "90b1f3c", by: "you", from: "Understand", when: "Nov 6", state: "", note: "rule: Never edit web/ and money.py in the same change", sessions: "2", success: "2/2", prompts: "4.0", graph: "●" },
  { v: "1.1", sha: "e44a812", by: "Debug", from: "Fix, turn 5 of totals", when: "Nov 4", state: "", note: "pin “Money is integer cents everywhere” to money.py", sessions: "1", success: "1/1", prompts: "5.0", graph: "●" },
  { v: "1.0", sha: "1f0c3d7", by: "Workbench", from: "defaults", when: "Nov 3", state: "", note: "worktree defaults, ChloeCtx wired, guarded commands", sessions: "0", success: "–", prompts: "–", graph: "●" },
];

/** Where the harness cost time or quality this week (harness_findings), worst first. */
export const findings = [
  { kind: "Permission storm", cost: "17m 40s", what: "<code>PYTHONPATH=. python3 -m pytest</code> asked 22 times. The rule <code>Bash(python3 -m pytest:*)</code> never matches it.", sessions: 9, agents: "cents, freeship, flakehunt", level: "counted", tone: "warn" },
  { kind: "Missed memory", cost: "11m 05s", what: "“Shipping is not taxed” scored 0.58 under 0.60 at freeship's turn 2; 2 tests failed after it.", sessions: 1, agents: "freeship", level: "recomputed", tone: "bad" },
  { kind: "Redo loop", cost: "7m 20s", what: "cents ran <code>test_money.py</code> 6 times on one float error; ChloeCtx had the fix from totals (Nov 9).", sessions: 2, agents: "cents, totals", level: "counted", tone: "warn" },
  { kind: "Corrected by you", cost: "3 times", what: "“Run it from the repository root”, said to 3 agents after they ran tests from <code>tests/</code>.", sessions: 3, agents: "cents, coupons-v2, refunds", level: "counted", tone: "" },
  { kind: "Context unused", cost: "210k tok", what: "62% of the briefing's tokens were never read back in the turn that got them.", sessions: 11, agents: "all", level: "counted", tone: "" },
  { kind: "Slow hook", cost: "4m 10s", what: "chloectx's <code>UserPromptSubmit</code>: p95 1.9 s on every prompt; 4 cold starts after sleep.", sessions: 14, agents: "all", level: "counted", tone: "" },
];

/** The suggestions of run 7 (suggestions). */
export const suggestions = [
  {
    id: "s1", part: "Permissions", title: "Allow the test command agents actually type",
    diff: [["+", "allow  Bash(PYTHONPATH=. python3 -m pytest:*)"]],
    evidence: "22 prompts in 9 sessions, 3 agents; you allowed all 22.",
    impact: "−18 of 22 prompts · 0 denies changed", level: "counted", widens: true, result: "won", res: "−2.2 prompts a session",
    alt: "Or add <code>pythonpath = .</code> to pytest.ini: agents then need no prefix (a change to the repository, through Review).",
  },
  {
    id: "s2", part: "Memory", title: "Pin your decisions to the files they name",
    diff: [["+", "pin    decision · by you · anchored → given when its file is edited"]],
    evidence: "freeship turn 2 missed “Shipping is not taxed” (cart.py). The same miss at refunds turn 3.",
    impact: "+5 records in 4 sessions · +38 tok a turn there", level: "recomputed", widens: false, result: "won", res: "+2 sessions pass",
    alt: "Tried and dropped: lowering the threshold to 0.50 fixes the same 2 turns with 4 times the extra tokens.",
  },
  {
    id: "s3", part: "Memory", title: "Halve the briefing a session starts with",
    diff: [["-", "briefing_chars  4000"], ["+", "briefing_chars  2000"]],
    evidence: "62% of its tokens were never read back, in 11 sessions.",
    impact: "−9k tok a session · success unchanged", level: "replayed", widens: false, result: "inconclusive", res: "inside the noise",
    alt: "The interval of tokens saved crosses zero after 3 runs a session. 5 more runs would take about $4.10.",
  },
];

/** Run 7: the sessions reproduced, 3 runs a version (eval_runs). p pass, f fail, r running, q queued. */
export const runRows = [
  { s: "freeship", task: "Coupon FREESHIP waives the shipping line", set: "train", turns: 3, a: "fff", b: "ppr", live: "fail → pass" },
  { s: "cents", task: "The total is a cent off from the receipt", set: "train", turns: 9, a: "ppp", b: "ppp", live: "−3 prompts" },
  { s: "flakehunt", task: "Make test_promo_window deterministic", set: "train", turns: 5, a: "ppf", b: "pp", live: "−2 prompts" },
  { s: "totals", task: "Tax per line, in integer cents", set: "train", turns: 11, a: "ppp", b: "pr", live: "−4 prompts" },
  { s: "coupons-v2", task: "Stackable coupons, one per kind", set: "train", turns: 8, a: "ppp", b: "pq", live: "" },
  { s: "receipt-pdf", task: "A PDF receipt for each order", set: "train", turns: 12, a: "fff", b: "fq", live: "" },
  { s: "refunds", task: "Refund a line; refunds are not taxed", set: "held-out", turns: 7, a: "fpf", b: "qq", live: "" },
  { s: "ledger-export", task: "Export the ledger as CSV for finance", set: "held-out", turns: 6, a: "ppp", b: "qq", live: "" },
  { s: "promo-dates", task: "Promotions start and end in the shop's time zone", set: "held-out", turns: 5, a: "ppp", b: "qq", live: "" },
  { s: "i18n-prices", task: "Prices in the customer's locale", set: "held-out", turns: 6, a: "ppp", b: "qq", live: "" },
];

/** The scorecard, A (1.4) against B (1.5-rc), over 10 sessions × 3 runs. */
export const score = [
  { k: "Task success", sub: "pass³: all 3 runs pass the checks", a: "6/10", b: "8/10", d: "+2", ci: "+1 … +3", v: "win" },
  { k: "Runs passing", sub: "of 30 runs", a: "21", b: "26", d: "+5", ci: "+2 … +8", v: "win" },
  { k: "Interventions", sub: "prompts, answers, steers a session", a: "3.1", b: "0.9", d: "−2.2", ci: "−2.9 … −1.5", v: "win" },
  { k: "Permission prompts", sub: "a session, counted", a: "2.6", b: "0.4", d: "−2.2", ci: "exact", v: "win" },
  { k: "Wall time", sub: "agent active, median", a: "14m 10s", b: "11m 40s", d: "−2m 30s", ci: "−4m 10s … −0m 50s", v: "win" },
  { k: "Turns", sub: "a session", a: "7.2", b: "6.1", d: "−1.1", ci: "−2.0 … −0.2", v: "win" },
  { k: "Tokens", sub: "a session", a: "128k", b: "117k", d: "−11k", ci: "−24k … +3k", v: "noise" },
  { k: "Cost", sub: "a session", a: "$0.41", b: "$0.38", d: "−$0.03", ci: "−$0.08 … +$0.01", v: "noise" },
  { k: "Tests passing at the end", sub: "the cases' checks", a: "94%", b: "100%", d: "+6 pts", ci: "+2 … +9", v: "win" },
  { k: "Held-out regressions", sub: "4 sessions not used to propose", a: "–", b: "0", d: "0", ci: "2 win · 2 tie", v: "win" },
];
