// Trace (SPEC.md, "Trace (as built)"): a question in a sentence, answered
// with a dossier of where it happened, for a demo repo "shop" (a Rust API
// in api/, a React app in web/). The answers have the shapes of the real
// routes: GET /api/trace, POST /api/trace/explain, GET /api/trace/commit,
// and GET /api/time (time travel, answered here so the Mission and Map
// links show).
import { NOW } from "./base.mjs";

const SHOP = "/home/user/src/shop";
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/shop-7e1d02aa";
// A time `d` days ago at h:m, local.
const at = (d, h, m, s = 0) => { const x = new Date(NOW - d * 864e5); x.setHours(h, m, s, 0); return x.getTime(); };
const iso = (ms) => new Date(ms).toISOString();

function agent(id, name, o) {
  return {
    id, name, repo: SHOP, cwd: `${WT}/${name}`, branch: `wb/${name}`, parent: "a7c1a000", depth: 1, model: "sonnet",
    running: true, state: "working", state_since: iso(NOW - 120e3), created: iso(at(1, 18, 44)), started_ms: at(1, 18, 44), children: [],
    session_id: `c4a7${id.slice(-4)}-0000-4000-8000-000000000000`, base: "4be1f07c", goal: "g5d1c0a77", ...o,
  };
}
const lead = agent("a7c1a000", "lead", { parent: null, depth: 0, task: "Discount codes at checkout, end to end", state: "idle", created: iso(at(1, 18, 40)), started_ms: at(1, 18, 40) });
const cart = agent("a7c1a002", "cart", { task: "cart_total takes discounts, rounded per line", outcome: "o5d1c0a02", narration: "Running cargo test cart_total: 12 of 12 pass." });
const checkout = agent("a7c1a001", "checkout", { task: "Apply a discount code in the checkout summary", outcome: "o5d1c0a01", narration: "Wiring the code field into Summary.tsx." });

const T0 = at(5, 10, 2);                 // the first commit: totals in cents, floor
const GOAL = at(1, 18, 40);
const ASKED = at(1, 19, 3, 12), ANSWERED = at(1, 19, 5, 40);
const DECIDED = at(1, 19, 6, 5);
const EDIT = at(1, 19, 12, 4), COMMIT = at(1, 19, 20, 31);
const WEB_EDIT = at(1, 19, 31, 50), WEB_DECIDED = at(1, 19, 40, 12);
const MERGE = at(0, 9, 15);

const rewind = (a, t) => ({ agent: a.id, from_ms: Math.max(0, t - a.started_ms - 20000), to_ms: t - a.started_ms + 3000, at_ms: t });
const brief = (a) => a && { id: a.id, name: a.name, state: a.state, running: a.running, branch: a.branch, repo: a.repo, cwd: a.cwd, created: a.created };

export const TRACE = {
  question: "when and why did we change the cart total rounding from floor to half-up, and how does it work now?",
  repo: SHOP,
  reading: {
    kinds: ["when", "why", "how"], x: "floor", y: "half-up", topic: "cart total rounding",
    subjects: [], files: [], paths: ["api/"], words: ["cart", "total", "rounding"],
    needles: ["floor", "half-up"],
    chips: [
      { key: "x", text: "floor", label: "x" },
      { key: "y", text: "half-up", label: "y" },
      { key: "topic", text: "cart total rounding", label: "about" },
      { key: "in", text: "api/", label: "in" },
    ],
  },
  answer: {
    text: "The cart total rounding went from floor to half-up {when} in cart's edit to api/src/cart/total.rs (committed as 8e41b07), after you answered “Round half up” to its question about per-line discounts coming out a cent low.",
    when_ms: EDIT,
    plain: "",
    key: "e-8e41b07",
    evidence: { commits: 3, edits: 1, decisions: 3, questions: 1, rules: 1, goals: 1 },
  },
  timeline: [
    {
      id: "c-3f2a9c1", kind: "commit", ts: iso(T0), ts_ms: T0, agent: null, by: "Dana Okafor", change: "added",
      title: "cart: totals in integer cents",
      text: "floor came in with the first cents-based cart_total.",
      commit: { sha: "3f2a9c1d07e4b6a58c3e2f1a09b8c7d6e5f4a3b2", short: "3f2a9c1", subject: "cart: totals in integer cents", author: "Dana Okafor", branches: ["main"], files: ["api/src/cart/total.rs"] },
      lines: [
        { path: "api/src/cart/total.rs", sign: "+", line: 24, text: "        let off = (gross as f64 * d.pct / 100.0).floor() as Cents;" },
      ],
      links: { story: null, rewind: null, why: null, diff: { sha: "3f2a9c1d07e4b6a58c3e2f1a09b8c7d6e5f4a3b2" }, time_ms: T0 },
    },
    {
      id: "g-g5d1c0a77-a7c1a002", kind: "goal", ts: iso(GOAL), ts_ms: GOAL, agent: brief(cart), by: "you",
      title: "Discount codes at checkout, end to end",
      text: "cart worked on the outcome “cart_total takes discounts, rounded per line”.",
      goal: { id: "g5d1c0a77", text: "Discount codes at checkout, end to end", state: "active", outcome: { id: "o5d1c0a02", title: "cart_total takes discounts, rounded per line", state: "done" } },
      links: { story: { agent: cart.id, at_ms: GOAL }, rewind: null, why: null, diff: null, time_ms: GOAL },
    },
    {
      id: "q-q7f30a1c2", kind: "question", ts: iso(ANSWERED), ts_ms: ANSWERED, agent: brief(cart), by: "you",
      title: "Per-line discounts come out a cent low on 3 of 40 fixture carts (floor). Round half up, or keep floor and fix the fixtures?",
      text: "You answered “Round half up” 2 min after it asked.",
      question: {
        id: "q7f30a1c2", text: "Per-line discounts come out a cent low on 3 of 40 fixture carts (floor). Round half up, or keep floor and fix the fixtures?",
        context: "cart_total([3 x $3.33, 15% off]) gives 849, the fixture says 850.", answer: "Round half up", answered_by: "you",
        options: [{ label: "Round half up", detail: "Matches the receipts the payment provider prints." }, { label: "Keep floor", detail: "Change 3 fixtures; customers pay a cent more." }],
        asked: iso(ASKED), asked_ms: ASKED, rule: "r5d1c0a09",
      },
      links: { story: { agent: cart.id, at_ms: ASKED }, rewind: rewind(cart, ASKED), why: null, diff: null, time_ms: ANSWERED },
    },
    {
      id: "r-r5d1c0a09", kind: "rule", ts: iso(ANSWERED + 900), ts_ms: ANSWERED + 900, agent: null, by: "you",
      title: "Money rounds half up, per line, in integer cents.",
      text: "Made from your answer; every agent of shop is told it.",
      rule: { id: "r5d1c0a09", text: "Money rounds half up, per line, in integer cents.", active: true, question: "q7f30a1c2", ctx_id: "c4a7a002:57" },
      links: { story: null, rewind: null, why: null, diff: null, time_ms: ANSWERED + 900 },
    },
    {
      id: "d-c4a7a002:61", kind: "decision", ts: iso(DECIDED), ts_ms: DECIDED, agent: brief(cart), by: "cart",
      title: "Round each line's discount half up before summing; floor lost a cent on 3 of 40 carts.",
      text: "",
      ctx: { id: "c4a7a002:61", kind: "decision", session: cart.session_id, superseded: false, files: ["api/src/cart/total.rs", "api/src/money.rs"], via: ["ask", "blame"] },
      links: { story: { agent: cart.id, at_ms: DECIDED }, rewind: rewind(cart, DECIDED), why: null, diff: null, time_ms: DECIDED },
    },
    {
      id: "e-8e41b07", kind: "commit", ts: iso(COMMIT), ts_ms: COMMIT, agent: brief(cart), by: "cart", change: "changed", key: true,
      title: "cart: round per-line discounts half up (was floor)",
      text: "floor became half-up in api/src/cart/total.rs, and round_half_up came in with it.",
      commit: { sha: "8e41b07a6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f", short: "8e41b07", subject: "cart: round per-line discounts half up (was floor)", author: "cart (Workbench)", branches: ["wb/cart", "main"], files: ["api/src/cart/total.rs", "api/src/money.rs", "api/tests/cart_total.rs"] },
      written: { ts: iso(EDIT), ts_ms: EDIT, tool: "Edit", file: "api/src/cart/total.rs" },
      lines: [
        { path: "api/src/cart/total.rs", sign: "-", line: 24, text: "        let off = (gross as f64 * d.pct / 100.0).floor() as Cents;" },
        { path: "api/src/cart/total.rs", sign: "+", line: 24, text: "        let off = round_half_up(gross * d.pct_bp, 10_000);" },
        { path: "api/src/money.rs", sign: "+", line: 14, text: "pub fn round_half_up(n: i64, d: i64) -> Cents {" },
        { path: "api/tests/cart_total.rs", sign: "+", line: 41, text: "    assert_eq!(cart_total(&three_at_333(), &[pct(15)]), 850); // half up, not 849" },
      ],
      links: { story: { agent: cart.id, at_ms: EDIT }, rewind: rewind(cart, EDIT), why: { repo: SHOP, path: "api/src/cart/total.rs", line: 24 }, diff: { sha: "8e41b07a6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f" }, time_ms: EDIT },
    },
    {
      id: "w-a7c1a001-3810", kind: "edit", ts: iso(WEB_EDIT), ts_ms: WEB_EDIT, agent: brief(checkout), by: "checkout", change: "changed", committed: false,
      title: "Edit web/src/cart/useCart.ts",
      text: "Not committed yet: in checkout's worktree.",
      lines: [
        { path: "web/src/cart/useCart.ts", sign: "-", line: 46, text: "    total += line.unitPrice * line.quantity - Math.floor(off);" },
        { path: "web/src/cart/useCart.ts", sign: "+", line: 46, text: "    total += line.unitPrice * line.quantity - roundHalfUp(off);" },
      ],
      links: { story: { agent: checkout.id, at_ms: WEB_EDIT }, rewind: rewind(checkout, WEB_EDIT), why: { agent: checkout.id, path: "web/src/cart/useCart.ts", line: 46 }, diff: null, time_ms: WEB_EDIT },
    },
    {
      id: "d-c4a7a001:88", kind: "decision", ts: iso(WEB_DECIDED), ts_ms: WEB_DECIDED, agent: brief(checkout), by: "checkout",
      title: "The web total mirrors the API: per-line discounts rounded half up, so the summary never disagrees with the charge.",
      text: "",
      ctx: { id: "c4a7a001:88", kind: "decision", session: checkout.session_id, superseded: false, files: ["web/src/cart/useCart.ts"], via: ["sql"] },
      links: { story: { agent: checkout.id, at_ms: WEB_DECIDED }, rewind: rewind(checkout, WEB_DECIDED), why: null, diff: null, time_ms: WEB_DECIDED },
    },
    {
      id: "c-c51d2e0", kind: "commit", ts: iso(MERGE), ts_ms: MERGE, agent: brief(lead), by: "lead", change: "touched",
      title: "cart: discount codes reach checkout (merge wb/cart)",
      text: "half-up reached main with the merge of wb/cart.",
      commit: { sha: "c51d2e0b9a8f7e6d5c4b3a291807f6e5d4c3b2a1", short: "c51d2e0", subject: "cart: discount codes reach checkout (merge wb/cart)", author: "lead (Workbench)", branches: ["main"], files: ["api/src/cart/total.rs", "api/src/money.rs"] },
      lines: [
        { path: "api/src/cart/total.rs", sign: "+", line: 24, text: "        let off = round_half_up(gross * d.pct_bp, 10_000);" },
      ],
      links: { story: { agent: lead.id, at_ms: MERGE }, rewind: rewind(lead, MERGE), why: null, diff: { sha: "c51d2e0b9a8f7e6d5c4b3a291807f6e5d4c3b2a1" }, time_ms: MERGE },
    },
  ],
  now: {
    source: "code_chunks",
    pack: { budget: 1200, used: 1104 },
    code: [
      {
        path: "api/src/money.rs", start_line: 10, end_line: 21, symbol: "round_half_up", tree: "main", agent: null, hits: [10, 14],
        text: `/// n / d rounded half up (away from zero for a negative n): 849.5 is 850,
/// -849.5 is -850. Money is i64 cents, never floats.
///
/// Per-line discounts use it since the rounding rule (half up, per line).
pub fn round_half_up(n: i64, d: i64) -> Cents {
    let q = n / d;
    let r = n % d;
    if 2 * r.abs() >= d.abs() {
        return q + n.signum() * d.signum();
    }
    q
}`,
        why: { repo: SHOP, path: "api/src/money.rs", line: 14 },
      },
      {
        path: "api/src/cart/total.rs", start_line: 18, end_line: 29, symbol: "cart_total", tree: "main", agent: null, hits: [24],
        text: `/// The cart's total in cents: each line's price times its quantity, less
/// the line's discount rounded half up, then the cart-wide discount.
pub fn cart_total(cart: &Cart, discounts: &[Discount]) -> Cents {
    let mut total: Cents = 0;
    for line in &cart.lines {
        let gross = line.unit_price * line.quantity as i64;
        let off = round_half_up(gross * d.pct_bp, 10_000);
        total += gross - off;
    }
    let cart_off = discounts.iter().filter(|d| d.scope == Scope::Cart).map(|d| d.amount(total)).sum::<Cents>();
    (total - cart_off).max(0)
}`,
        why: { repo: SHOP, path: "api/src/cart/total.rs", line: 24 },
      },
    ],
    docs: [
      { title: "RoundingStrategy in rust_decimal", url: "https://docs.rs/rust_decimal/1.36.0/rust_decimal/enum.RoundingStrategy.html", lib: "rust_decimal", excerpt: "MidpointAwayFromZero: when a number is halfway between two others, it is rounded toward the nearest number that is away from zero. e.g. 6.5 -> 7, -6.5 -> -7." },
    ],
  },
  links: { time_ms: EDIT, story: { agent: cart.id, at_ms: EDIT }, rewind: rewind(cart, EDIT) },
  missing: [],
  ms: 412,
};

const EXPLAIN = {
  text: "",
  answer: "Totals used to floor each line's percentage discount (3f2a9c1, five days ago), which made 3 of the 40 fixture carts a cent low [E1]. cart asked you whether to round half up or fix the fixtures [E3]; you chose half up, and that answer became the rule “Money rounds half up, per line, in integer cents” [E4]. cart then wrote round_half_up in api/src/money.rs and used it in cart_total [E6], committed as 8e41b07, and checkout mirrored it in the web total so the summary matches the charge [E7, E8]. Now each line's discount is n / d rounded half up in integer cents (849.5 is 850) before the cart-wide discount is taken.",
  confidence: 0.86, cost_usd: 0.012, ids: ["c4a7a002:61", "c4a7a001:88", "c4a7a002:57"], ms: 9120,
  evidence: ["E1 commit 3f2a9c1", "E3 question q7f30a1c2", "E4 rule r5d1c0a09", "E6 commit 8e41b07", "E7 edit useCart.ts", "E8 decision c4a7a001:88"],
  missing: [],
};

const COMMIT_DIFF = {
  sha: "8e41b07a6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f", short: "8e41b07", subject: "cart: round per-line discounts half up (was floor)", author: "cart (Workbench)", ts: iso(COMMIT),
  files: [{ path: "api/src/cart/total.rs", status: "modified", hunks: [{ old_start: 21, new_start: 21, lines: [
    { kind: " ", text: "    for line in &cart.lines {", new_line: 21 },
    { kind: " ", text: "        let gross = line.unit_price * line.quantity as i64;", new_line: 22 },
    { kind: " ", text: "        for d in discounts.iter().filter(|d| d.applies_to(line)) {", new_line: 23 },
    { kind: "-", text: "        let off = (gross as f64 * d.pct / 100.0).floor() as Cents;", new_line: null },
    { kind: "+", text: "        let off = round_half_up(gross * d.pct_bp, 10_000);", new_line: 24 },
    { kind: " ", text: "        total += gross - off;", new_line: 25 },
  ] }] }, { path: "api/src/money.rs", status: "modified", hunks: [{ old_start: 9, new_start: 9, lines: [
    { kind: "+", text: "/// n / d rounded half up (away from zero for a negative n).", new_line: 10 },
    { kind: "+", text: "pub fn round_half_up(n: i64, d: i64) -> Cents {", new_line: 14 },
    { kind: "+", text: "    let q = n / d;", new_line: 15 },
  ] }] }],
};

export const routes = [
  ["GET", /^\/api\/trace$/, (_, url) => {
    const q = url.searchParams.get("q") || "";
    if (!q.trim()) return [400, { error: "q is required", error_code: "bad_request" }];
    return { ...TRACE, question: q };
  }],
  ["POST", /^\/api\/trace\/explain$/, async () => { await new Promise((r) => setTimeout(r, 700)); return EXPLAIN; }],
  ["GET", /^\/api\/trace\/commit$/, () => COMMIT_DIFF],
  ["GET", /^\/api\/time$/, (_, url) => ({ at_ms: Number(url.searchParams.get("at")) || NOW, mission: {}, agents: [], missing: [] })],
];
