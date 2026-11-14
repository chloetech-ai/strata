// Story's routes (SPEC.md, "Story (one agent)"): ledger's 40 minutes on
// "Idempotent refunds" in four turns and twenty grouped steps, what ChloeCtx
// put in its context (and left out), two déjà vus, the events behind each
// step and the last seconds of its terminal. The other agents get a shorter
// story of the same shape, so every card on Mission opens to something.
import { ms, agents } from "./base.mjs";

const ESC = "\x1b[";
const c = (n, s) => `${ESC}${n}m${s}${ESC}0m`;

// [kind, label, detail, tool, calls (one summary each), seconds, failed]
const LEDGER = [
  { title: "Explored", by: "lead", prompt: "Make refunds idempotent so a retried request never pays twice", from: 40, to: 35.2,
    summary: "Found two refund paths: the API and the nightly batch. Only the API checked for duplicates.",
    steps: [
      ["memory", "Searched memory 2 times", "“refund idempotency”, “ledger entries”", "mcp__chloectx__memory_search", ["memory_search “refund idempotency”: 4 records", "memory_search “ledger entries”: 3 records"], 0.4],
      ["read", "Read 12 files", "src/ledger/*, src/refunds/*, migrations/0031_charges_idem.sql …", "Read", ["src/ledger/mod.rs", "src/ledger/post.rs", "src/ledger/entry.rs", "src/refunds/mod.rs", "src/refunds/service.rs", "src/refunds/model.rs", "src/refunds/api.rs", "src/charges/idem.rs", "src/charges/service.rs", "migrations/0031_charges_idem.sql", "migrations/0038_refunds.sql", "src/db.rs"], 11],
      ["search", "Searched code 3 times", "“idempotency_key”, “fn create_refund”, “nightly”", "Grep", ["Grep idempotency_key: 9 matches in 4 files", "Grep fn create_refund: 2 matches", "Grep nightly: 6 matches in src/batch"], 1.2],
      ["read", "Read 3 files", "src/batch/nightly.rs, src/batch/claim.rs, src/batch/mod.rs", "Read", ["src/batch/nightly.rs", "src/batch/claim.rs", "src/batch/mod.rs"], 3.5],
      ["run", "Ran git log ×2", "git log --oneline -- src/charges/idem.rs", "Bash", ["git log --oneline -- src/charges/idem.rs", "git show 4be19c2 --stat"], 0.8],
    ] },
  { title: "Decided", by: "lead", prompt: "Go with what charges does, if it fits.", from: 35, to: 33.6,
    summary: "Key on (merchant, client_ref), the same key charges use, instead of a new UUID.",
    steps: [
      ["memory", "Searched memory 3 times", "“idempotency”, “refund race”, “batch worker claim”", "mcp__chloectx__memory_search", ["memory_search “idempotency”: 5 records", "memory_search “refund race”: 1 record", "memory_search “batch worker claim”: 2 records"], 0.4],
      ["read", "Read 2 files", "src/charges/idem.rs, migrations/0031_charges_idem.sql", "Read", ["src/charges/idem.rs", "migrations/0031_charges_idem.sql"], 1.1],
      ["agents", "Told lead the plan", "send_to_agent lead: “keying on (merchant, client_ref)”", "mcp__workbench__send_to_agent", ["send_to_agent lead: keying on (merchant, client_ref), as charges does"], 0.2],
    ] },
  { title: "Built", by: "lead", prompt: "Sounds right. Build it.", from: 33.4, to: 15,
    summary: "Migration 0042 adds `refunds.idem_key` with a unique index; RefundService::create checks it.",
    steps: [
      ["edit", "Edited 2 files", "+38 −2 · migrations/0042_refunds_idem.sql (new), src/refunds/model.rs", "Write", ["Write migrations/0042_refunds_idem.sql (+21)", "Edit src/refunds/model.rs (+17 −2)"], 4],
      ["run", "Ran cargo sqlx prepare ×2", "1st: offline data missing · 2nd: ok", "Bash", ["cargo sqlx prepare --workspace: error: sqlx offline data missing", "cargo sqlx prepare --workspace: ok"], 26, 1],
      ["memory", "Recalled a fix", "“sqlx offline data missing” → cargo sqlx prepare", "mcp__chloectx__memory_search", ["memory_search “sqlx offline data missing”: 1 fix"], 0.3],
      ["edit", "Edited src/refunds/service.rs", "+64 −9 · RefundService::create looks the key up first", "Edit", ["Edit src/refunds/service.rs (+41 −6)", "Edit src/refunds/service.rs (+23 −3)"], 3],
      ["read", "Read 3 files", "src/refunds/api.rs, src/api/errors.rs, tests/common/mod.rs", "Read", ["src/refunds/api.rs", "src/api/errors.rs", "tests/common/mod.rs"], 1.6],
      ["edit", "Edited 2 files", "+80 −9 · src/refunds/batch.rs, tests/refunds_idem.rs (new)", "Edit", ["Edit src/refunds/batch.rs (+12 −9)", "Write tests/refunds_idem.rs (+68)"], 5],
      ["run", "Ran cargo build", "ok, 2 warnings", "Bash", ["cargo build: Finished dev profile, 2 warnings"], 41],
    ] },
  { title: "Testing", by: "lead", prompt: "Run the tests and fix what fails.", from: 14.8, to: null,
    summary: "2 of 3 tests pass. The batch test fails: two workers can claim one refund.",
    steps: [
      ["run", "Ran cargo test ×2", "1st: 1 failed · 2nd: 1 failed", "Bash", ["cargo test refunds: 2 passed, 1 failed (two_workers_one_refund)", "cargo test refunds: 2 passed, 1 failed (two_workers_one_refund)"], 130, 2],
      ["memory", "Searched memory", "“two workers claim one refund”: a fix from charges", "mcp__chloectx__memory_search", ["memory_search “two workers claim one refund”: 1 fix (déjà vu)"], 0.3],
      ["read", "Read src/charges/batch.rs", "lines 70–110: the claim with FOR UPDATE SKIP LOCKED", "Read", ["src/charges/batch.rs (70–110)"], 0.6],
      ["edit", "Edited src/refunds/batch.rs", "+6 −2 · the claim takes a row lock", "Edit", ["Edit src/refunds/batch.rs (+6 −2)"], 2],
      ["run", "Running cargo test refunds", "3rd run", "Bash", ["cargo test refunds"], null],
    ] },
];

const KNEW_LEDGER = [
  { id: "e3b1c2d4:100", kind: "decision", text: "Money is i64 cents, never floats.", score: 0.91, turn: 1, deja_vu: false, by: "you", ts: "2026-11-03T15:20:00Z", decision: "knowledge", tokens: 12 },
  { id: "9d4e7a10:58", kind: "decision", text: "Charges key idempotency on (merchant, client_ref).", score: 0.88, turn: 1, deja_vu: false, by: "charges idempotency", ts: "2026-11-09T14:02:00Z", decision: "knowledge", tokens: 14 },
  { id: "e3b1c2d4:107", kind: "decision", text: "Refunds are negative ledger entries, never a separate table.", score: 0.83, turn: 1, deja_vu: false, by: "you", ts: null, decision: "knowledge", tokens: 13 },
  { id: "9d4e7a10:73", kind: "fix", text: "Batch workers must claim with a row lock: `SELECT … FOR UPDATE SKIP LOCKED`.", score: 0.84, turn: 4, deja_vu: true, by: "charges idempotency", ts: "2026-11-09T15:40:00Z", decision: "take", tokens: 412 },
  { id: "e3b1c2d4:140", kind: "fix", text: "“sqlx offline data missing” → run `cargo sqlx prepare`.", score: 0.79, turn: 3, deja_vu: true, by: "refunds-api", ts: null, decision: "take", tokens: 96 },
  { id: "4f20aa91:9", kind: "fact", text: "Refund webhooks retry 5 times.", score: 0.52, turn: 2, deja_vu: false, by: "hooks", ts: null, decision: "skip: over budget", tokens: 380 },
];

const K = Object.fromEntries(KNEW_LEDGER.map((k) => [k.id, k]));
const KNEW_OTHERS = {
  "refunds-api": [K["e3b1c2d4:107"], K["9d4e7a10:58"], { ...K["e3b1c2d4:100"], decision: "skip: shown before" }],
  web: [{ id: "b4d08e51:3", kind: "decision", text: "Keep the charge as the unit. Disputes go under it.", score: 0.81, deja_vu: false, by: "you", ts: null, decision: "knowledge", tokens: 11 }, K["e3b1c2d4:100"]],
  hooks: [{ id: "e3b1c2d4:114", kind: "decision", text: "Webhooks retry 5 times with exponential backoff, as charge.updated does.", score: 0.86, deja_vu: false, by: "you", ts: null, decision: "knowledge", tokens: 16 }, { id: "c7e2a9f3:2", kind: "fact", text: "charge.updated is signed in src/webhooks/sign.rs.", score: 0.74, deja_vu: false, by: "hooks", ts: null, decision: "take", tokens: 40 }],
  lead: [K["e3b1c2d4:107"]],
};

function build(a) {
  const plan = a.name === "ledger" ? LEDGER : [
    { title: "Explored", by: a.parent ? "lead" : "you", prompt: a.task, from: (Date.now() - a.started_ms) / 60000, to: (Date.now() - a.started_ms) / 60000 - 6,
      summary: "Read how the neighbouring code does it and what memory says about it.",
      steps: [
        ["memory", "Searched memory 2 times", `“${a.task.split(" ").slice(0, 3).join(" ")}”`, "mcp__chloectx__memory_search", ["memory_search: 3 records", "memory_search: 1 record"], 0.4],
        ["read", "Read 6 files", "src/…", "Read", ["src/lib.rs", "src/api/mod.rs", "src/refunds/mod.rs", "src/charges/mod.rs", "src/webhooks/mod.rs", "README.md"], 5],
      ] },
    { title: a.state === "waiting" ? "Asked you" : "Building", by: "lead", prompt: "Carry on.", from: (Date.now() - a.started_ms) / 60000 - 6.2, to: null,
      summary: a.narration || "", steps: [
        ["edit", "Edited 3 files", "+54 −11", "Edit", ["Edit a", "Edit b", "Edit c"], 6],
        ["run", "Ran cargo test", "ok", "Bash", ["cargo test: ok"], 38],
      ] },
  ];
  let seq = a.name === "ledger" ? 1200 : 400 + agents.indexOf(a) * 100;
  const chapters = plan.map((ch, i) => {
    const started = ms(ch.from), ended = ch.to == null ? null : ms(ch.to);
    const span = (ended ?? Date.now()) - started;
    let t = started + 4000;
    const steps = ch.steps.map(([kind, label, detail, tool, calls, secs, failed]) => {
      const events = calls.map(() => (seq += 2));
      const s = { kind, label, detail, count: calls.length, started_ms: Math.round(t), ms: secs == null ? null : Math.round(secs * 1000), failed: failed || 0, events };
      // What the events route answers for this step; not part of the story's JSON.
      Object.defineProperties(s, { tool: { value: tool }, calls: { value: calls } });
      t += Math.max((secs || 20) * 1000, span / (ch.steps.length + 1));
      return s;
    });
    return { turn: i + 1, prompt: { text: ch.prompt, by: ch.by }, started_ms: started, ended_ms: ended, title: ch.title, summary: ch.summary, state: ch.to == null ? "now" : "done", steps };
  });
  const isLedger = a.name === "ledger";
  return {
    agent: a,
    intent: isLedger ? "Make refunds idempotent, so a retried request never pays a customer twice." : a.task,
    chapters,
    knew: isLedger ? KNEW_LEDGER : (KNEW_OTHERS[a.name] || KNEW_LEDGER.slice(0, 2)).map((k) => ({ ...k, turn: 1 })),
    deja_vu: isLedger ? [
      { text: "Session “charges idempotency” hit the same double claim and fixed it with `FOR UPDATE SKIP LOCKED` in `src/charges/batch.rs:88`.", record: "9d4e7a10:73", when: "2026-11-09T15:40:00Z", shown_ms: ms(9.4) },
      { text: "“sqlx offline data missing” was fixed by refunds-api: run `cargo sqlx prepare`.", record: "e3b1c2d4:140", when: new Date(ms(33)).toISOString(), shown_ms: ms(30) },
    ] : [],
    tokens: isLedger ? { input: 31200, output: 9800, cache_read: 412000 } : { input: 18400, output: 5100, cache_read: 160000 },
    // Assumed beyond SPEC.md: the receipts' totals, to say what was left out.
    budget: isLedger ? { candidates: 11, shown: 5, tokens: 1840, limit: 2000 } : null,
  };
}

const stories = new Map();
const storyOf = (a) => { if (!stories.has(a.id)) stories.set(a.id, build(a)); const s = stories.get(a.id); s.agent = a; return s; };

function events(a, after, limit) {
  const s = storyOf(a);
  const out = [];
  for (const ch of s.chapters) for (const st of ch.steps) st.events.forEach((seq, i) => {
    const ts = st.started_ms + i * Math.max(300, (st.ms || 4000) / st.count);
    const fail = st.failed && i < st.failed;
    out.push({ id: `${a.id}:${String(seq).padStart(8, "0")}`, seq, ts: new Date(ts).toISOString(), agent: a.id, kind: st.ms == null && i === st.count - 1 ? "PreToolUse" : "PostToolUse", tool: st.tool, summary: (fail ? "failed: " : "") + st.calls[i] });
  });
  return out.filter((e) => e.seq > after).slice(0, limit);
}

const SCREEN = [
  c(90, "running 3 tests"),
  `test refunds::create::rejects_a_second_key ... ${c(32, "ok")}`,
  `test refunds::create::same_key_same_answer ... ${c(32, "ok")}`,
  `test refunds::batch::two_workers_one_refund ... ${c(31, "FAILED")}`,
  "",
  `  assertion failed: left == right (left: 2 payouts, right: 1)`,
  "",
  `${c(34, "●")} The claim query needs a row lock. Applying the fix from charges (SELECT … FOR UPDATE SKIP LOCKED).`,
  "",
  `${c(34, "●")} ${c(1, "Update")}(src/refunds/batch.rs)`,
  `  ⎿  Updated ${c(1, "src/refunds/batch.rs")} with ${c(32, "6 additions")} and ${c(31, "2 removals")}`,
  "",
  `${c(34, "●")} ${c(1, "Bash")}(cargo test refunds)`,
  `  ⎿  ${c(90, "Running… (12s)")}`,
];

export const routes = [
  ["GET", /^\/api\/agents\/([^/]+)\/story$/, (m) => {
    const a = agents.find((x) => x.id === m[1]);
    return a ? storyOf(a) : [404, { error: `no agent ${m[1]}`, error_code: "not_found" }];
  }],
  ["GET", /^\/api\/agents\/([^/]+)\/events$/, (m, url) => {
    const a = agents.find((x) => x.id === m[1]);
    if (!a) return [404, { error: `no agent ${m[1]}`, error_code: "not_found" }];
    return { events: events(a, Number(url.searchParams.get("after") || 0), Math.min(500, Number(url.searchParams.get("limit") || 200))) };
  }],
  ["GET", /^\/api\/agents\/([^/]+)\/recording$/, (m, url) => {
    const a = agents.find((x) => x.id === m[1]);
    if (!a) return [404, { error: `no agent ${m[1]}`, error_code: "not_found" }];
    const from = Number(url.searchParams.get("from_ms") || 0);
    const lines = a.name === "ledger" ? SCREEN : [`${c(34, "●")} ${a.narration || "…"}`, "", `${c(90, "> ")}`];
    const half = Math.floor(lines.length / 2);
    return {
      width: 120, height: 32, started_ms: a.started_ms,
      before: `${ESC}2J${ESC}H` + lines.slice(0, half).join("\r\n") + "\r\n",
      events: lines.slice(half).map((l, i) => [from + 1000 * (i + 1), "o", l + "\r\n"]),
    };
  }],
];
