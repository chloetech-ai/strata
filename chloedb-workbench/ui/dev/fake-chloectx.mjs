#!/usr/bin/env node
// A stand-in for `chloectx`, for running the real Workbench server without
// ChloeCtx (`chloedb-workbench serve --chloectx-bin ui/dev/fake-chloectx.mjs`).
// It answers the commands Workbench runs, in the formats chloectx prints
// them (chloectx/src/bin/chloectx.rs, src/daemon.rs, src/report.rs):
//
//   ask --json [--] QUESTION          {text, ids, ms}
//   sql --json SQL                    {text, columns, rows, count}
//   receipts --session S [--limit N] --json   {text, receipts: [...]}
//   remember KIND TEXT                Stored KIND as ID.
//   blame PATH:LINE --json            {text, file, line, edit, prompt, notes, receipts, ...}
//   report --json [--days N]          the Report struct
//   wiring --project DIR --exe EXE    {version, exe, wired, settings, mcp, store, model}:
//                                     "already wired" (nothing to add), so a
//                                     Workbench that requires ChloeCtx starts sessions
//
// Its memory is a JSON file, $WB_FAKE_CHLOECTX_STORE (default
// fake-chloectx.json next to this script): {records: [{id, kind, tag, ts
// (seconds), session, turn, text, files, ctx, links}], receipts: [...],
// edits: [{path, text, session, turn, ts}], seq}. ./seed-real.mjs writes it.
// Node 22, standard library only.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const STORE = process.env.WB_FAKE_CHLOECTX_STORE || path.join(here, "fake-chloectx.json");
const args = process.argv.slice(2);
const flag = (f) => { const i = args.indexOf(f); if (i < 0) return false; args.splice(i, 1); return true; };
const opt = (f) => { const i = args.indexOf(f); if (i < 0) return null; const v = args[i + 1]; args.splice(i, 2); return v; };
const die = (msg) => { process.stderr.write(`chloectx: ${msg}\n`); process.exit(1); };
const out = (v) => process.stdout.write((typeof v === "string" ? v : JSON.stringify(v, null, 2)) + "\n");

function load() {
  try { return JSON.parse(fs.readFileSync(STORE, "utf8")); } catch { return { records: [], receipts: [], edits: [], seq: 100 }; }
}
function save(s) {
  const tmp = `${STORE}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(s, null, 1));
  fs.renameSync(tmp, STORE);
}
const now = () => Math.floor(Date.now() / 1000);
const date = (ts) => new Date(ts * 1000).toISOString().slice(0, 10);
const noteKind = (r) => (r.kind === "preference" ? "rule" : r.kind);
const words = (s) => String(s || "").toLowerCase().match(/[a-z0-9_]{3,}/g) || [];
const STOP = new Set(["the", "and", "for", "with", "not", "but", "you", "your", "this", "that", "what", "which", "how", "should", "does", "are", "was", "use", "one", "all", "any"]);

const cmd = args.shift();
switch (cmd) {
  case "ask": ask(); break;
  case "sql": sql(); break;
  case "receipts": receipts(); break;
  case "remember": remember(); break;
  case "blame": blame(); break;
  case "report": report(); break;
  case "--version": out("chloectx 0.0.0-fake"); break;
  case "wiring": out({ version: "0.0.0-fake", exe: opt("--exe") || process.argv[1], wired: "user", settings: {}, mcp: {}, store: path.dirname(STORE), model: { ok: true, embedder: "fake", path: STORE, fix: null } }); break;
  default: die(`unknown command ${cmd || "(none)"} (this is Workbench's fake chloectx)`);
}

// ------------------------------------------------------------------ ask
// As op_ask without a model: the notes whose words the question shares,
// as context::build lists them ("- date kind: text"), and their ids.
function ask() {
  flag("--json"); flag("--deep");
  if (args[0] === "--") args.shift();
  const q = args.join(" ").trim();
  if (!q) die("question is empty");
  const s = load();
  const want = new Set(words(q).filter((w) => !STOP.has(w)));
  const scored = s.records
    .filter((r) => ["decision", "preference", "fact", "error"].includes(r.kind) && r.tag !== "superseded")
    .map((r) => ({ r, n: words(r.text).filter((w) => want.has(w)).length }))
    .filter((x) => x.n >= 2 || (x.n >= 1 && want.size <= 3))
    .sort((a, b) => b.n - a.n || b.r.ts - a.r.ts)
    .slice(0, 6)
    .map((x) => x.r)
    .sort((a, b) => a.ts - b.ts);
  if (!scored.length) return out({ text: "ChloeCtx found nothing relevant in memory.", ids: [], ms: 3 });
  let text = "Notes from earlier conversations with the user, in their current versions, oldest first (date, kind, note; what it replaced in brackets). Rules come first; a later note can change what an earlier one says:\n";
  for (const r of scored) text += `- ${date(r.ts)} ${noteKind(r)}: ${r.text.replace(/\s+/g, " ")}\n`;
  out({ text: text + "\n", ids: scored.map((r) => r.id), ms: 4 });
}

// ------------------------------------------------------------------ sql
// SELECT <cols> FROM memory [WHERE <expr>] [ORDER BY <col> ASC|DESC] [LIMIT n],
// the WHERE of =, <>, >=, <=, >, <, IN (...), AND, OR, NOT and parentheses.
function sql() {
  const json = flag("--json");
  const text = args.join(" ").trim();
  const m = text.match(/^SELECT\s+(.+?)\s+FROM\s+memory(?:\s+WHERE\s+(.+?))?(?:\s+ORDER\s+BY\s+(\w+)(?:\s+(ASC|DESC))?)?(?:\s+LIMIT\s+(\d+))?\s*;?$/is);
  if (!m) die(`this fake reads only SELECT … FROM memory: ${text.slice(0, 80)}`);
  const cols = m[1].split(",").map((c) => c.trim());
  const s = load();
  let rows = s.records.filter(m[2] ? where(m[2]) : () => true);
  if (m[3]) { const k = m[3], d = (m[4] || "ASC").toUpperCase() === "DESC" ? -1 : 1; rows.sort((a, b) => (a[k] > b[k] ? d : a[k] < b[k] ? -d : 0)); }
  if (m[5]) rows = rows.slice(0, Number(m[5]));
  const cells = rows.map((r) => cols.map((c) => (c === "*" ? r : r[c] ?? null)));
  const table = [cols.join(" | "), ...cells.map((r) => r.map((v) => (v == null ? "NULL" : String(v))).join(" | "))].join("\n");
  if (!json) return out(table);
  out(JSON.stringify({ text: `${table}\n(${rows.length} rows)`, columns: cols, rows: cells.slice(0, 1000), count: rows.length }));
}

function where(expr) {
  const toks = expr.match(/'(?:[^']|'')*'|\d+(?:\.\d+)?|<>|!=|>=|<=|[=<>(),]|\w+/g) || [];
  let i = 0;
  const peek = () => toks[i], next = () => toks[i++];
  const lit = (t) => (t[0] === "'" ? t.slice(1, -1).replace(/''/g, "'") : /^\d/.test(t) ? Number(t) : null);
  function value(r) {
    const t = next();
    if (t === "(") { const v = value(r); next(); return v; }
    const l = lit(t);
    if (l !== null) return l;
    if (/^null$/i.test(t)) return null;
    return r[t] ?? null;
  }
  function cmp(r) {
    if (/^not$/i.test(peek())) { next(); return !cmp(r); }
    if (peek() === "(" ) {
      // A parenthesised condition (a value in parentheses is not used here).
      next(); const v = or(r); next(); return v;
    }
    const a = value(r);
    const op = next();
    if (/^in$/i.test(op)) {
      next(); const vs = [];
      while (peek() !== ")") { vs.push(value(r)); if (peek() === ",") next(); }
      next();
      return vs.includes(a);
    }
    if (/^is$/i.test(op)) { const neg = /^not$/i.test(peek()) ? (next(), true) : false; next(); return neg ? a != null : a == null; }
    const b = value(r);
    switch (op) {
      case "=": return a === b || (a != null && b != null && String(a) === String(b));
      case "<>": case "!=": return String(a) !== String(b);
      case ">=": return a >= b; case "<=": return a <= b; case ">": return a > b; case "<": return a < b;
      default: die(`cannot read the condition at ${op}`);
    }
  }
  function and(r) { let v = cmp(r); while (/^and$/i.test(peek() || "")) { next(); const w = cmp(r); v = v && w; } return v; }
  function or(r) { let v = and(r); while (/^or$/i.test(peek() || "")) { next(); const w = and(r); v = v || w; } return v; }
  return (r) => { i = 0; return or(r); };
}

// ------------------------------------------------------------- receipts
function receipts() {
  flag("--json");
  const session = opt("--session");
  const limit = Number(opt("--limit") || 20);
  const s = load();
  const list = (s.receipts || []).filter((r) => !session || r.session.startsWith(session)).sort((a, b) => b.ts - a.ts).slice(0, limit);
  const text = list.map((r) => `${r.id}  ${new Date(r.ts * 1000).toISOString().slice(0, 16).replace("T", " ")}  session ${r.session.slice(0, 8)} turn ${r.turn} ${r.source} ${r.decision || "-"} ${r.shown.length} shown, ${r.tokens} tokens · ${r.query.slice(0, 60)}`).join("\n");
  out({ text: text || "No receipts yet.", receipts: list });
}

// ------------------------------------------------------------- remember
function remember() {
  const kind = args.shift();
  const text = args.join(" ").trim();
  if (!text) die("text is empty");
  const s = load();
  s.seq = (s.seq || 100) + 1;
  const id = `manual00:${s.seq}`;
  s.records.push({ id, kind, tag: "remembered", ts: now(), session: "manual", turn: 0, text, files: "", ctx: "", links: "" });
  save(s);
  out(`Stored ${kind} as ${id}.`);
}

// ---------------------------------------------------------------- blame
// The edit whose text is the line's (from the store's edits), its turn's
// prompt and notes, and what was shown before it.
function blame() {
  flag("--json"); flag("--no-replay");
  const target = args[0] || die("blame needs a target: path or path:line");
  const mm = target.match(/^(.*?)(?::(\d+))?$/);
  const file = mm[1], line = mm[2] ? Number(mm[2]) : null;
  const abs = path.resolve(file);
  let lineText = null;
  try { if (line) lineText = fs.readFileSync(abs, "utf8").split("\n")[line - 1] ?? null; } catch {}
  const s = load();
  const rel = path.relative(process.cwd(), abs);
  const e = lineText == null ? null : (s.edits || []).filter((x) => rel.endsWith(x.path) && lineText.trim() && x.text.trim() === lineText.trim()).sort((a, b) => b.ts - a.ts)[0];
  if (!e) return out({ text: `${rel}:${line}\nno recorded edit wrote this line`, file: rel, line, line_text: lineText, git: null, how: "no recorded edit with this text", edit: null, edits: 0 });
  const turn = s.records.filter((r) => r.session === e.session && r.turn === e.turn);
  const notes = turn.filter((r) => ["decision", "preference", "fact"].includes(r.kind));
  const rcs = (s.receipts || []).filter((r) => r.session === e.session && r.ts <= e.ts).slice(-3);
  out({
    text: `${rel}:${line}\nedit ${date(e.ts)} · session ${e.session.slice(0, 8)} turn ${e.turn}`,
    file: rel, line, line_text: lineText, git: null, how: "the edit's new text holds the line",
    edit: { id: `${e.session.replace(/[^a-z0-9]/gi, "").slice(0, 8)}:e${e.turn}`, session: e.session, turn: e.turn, ts: e.ts, tool: "Edit", file: e.path },
    prompt: e.prompt ? { text: e.prompt, ts: e.ts - 20 } : null,
    calls: [], notes: notes.map((r) => ({ id: r.id, kind: r.kind, text: r.text, ts: r.ts })),
    receipts: rcs.map((r) => ({ receipt: r.id, turn: r.turn, ts: r.ts, rows: r.rows.map((x) => ({ row: { id: x.id, kind: x.kind, score: x.score, decision: x.decision }, text: x.text, now: [] })) })),
    replays: [], causes: [], edits: 1,
  });
}

// --------------------------------------------------------------- report
function report() {
  flag("--json");
  const days = Number(opt("--days") || 1);
  const t = now();
  out({
    project: path.basename(process.cwd()), days, since: t - days * 86400, until: t, resolution: "seconds", wait_cap_secs: 1800,
    total: { model: 1260, tools: 840, waiting: 410, permission: 190, compaction: 60, away: 0 },
    turns: 34, tool_calls: 212, timed_calls: 198, compactions: 1, compactions_per_hour: 0.4, permission_prompts: 6, subagents: 0,
    sessions: [], commands: [{ command: "cargo test -p ledger", runs: 9, timed: 9, total_secs: 312, max_secs: 61, failed: 4 }],
    tools: [{ tool: "Bash", calls: 61, total_secs: 702 }, { tool: "Read", calls: 88, total_secs: 41 }],
    errors: [], rereads: [],
    suggestions: [
      { kind: "slow_command", text: "`cargo test -p ledger` took 312 s over 9 runs; run the one test you changed (`cargo test -p ledger refund`) while iterating.", seconds: 312 },
      // As chloectx's report words it (chloectx/src/report.rs).
      { kind: "permission", text: "Claude asked for permission 6 times (Bash 6 times) and waited 3m10s in all for the answers: allowing the ones you always approve in the project's `.claude/settings.json` (`permissions.allow`) would spare the waits.", seconds: 190 },
      { kind: "compaction", text: "One session compacted after 2 h; start a fresh session per outcome rather than one long one.", seconds: 60 },
    ],
  });
}
