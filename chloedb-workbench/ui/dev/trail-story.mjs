// The decision trail's fake data for mock-server.mjs: one believable story,
// told consistently by every route of SPEC.md's "Decision trail" section.
//
// In /home/user/src/invoicer (a small TypeScript invoicing service), you
// asked `lead` to fix invoices that come out a cent off (#412). lead decided
// that money is integer cents, then spawned two workers: `fix-total`
// (totals, tax rounded half up on each line) and `add-validate` (strict
// input). lead later corrected add-validate (refuse a third decimal rather
// than round it) and asked fix-total for a rename, and you asked lead for an
// ADR. Decisions supersede earlier ones, two of them from sessions outside
// Workbench. Each session has a recording (asciicast-like events) with a
// Claude Code-like screen, the edits landing at the times the cards say.

const E = "\x1b[";
const orange = (s) => `${E}38;2;215;119;87m${s}${E}0m`;
const gray = (s) => `${E}90m${s}${E}0m`;
const bold = (s) => `${E}1m${s}${E}22m`;
const green = (s) => `${E}32m${s}${E}0m`;
const red = (s) => `${E}31m${s}${E}0m`;
const blue = (s) => `${E}38;2;87;150;230m${s}${E}0m`;
const visible = (s) => [...s.replace(/\x1b\[[0-9;]*m/g, "")].length;

function box(lines, w, color = gray) {
  const pad = (s) => s + " ".repeat(Math.max(0, w - 4 - visible(s)));
  return [color(`╭${"─".repeat(w - 2)}╮`), ...lines.map((l) => `${color("│")} ${pad(l)} ${color("│")}`), color(`╰${"─".repeat(w - 2)}╯`)].join("\n") + "\n";
}

export const STORY_REPO = "/home/user/src/invoicer";
const BASE = "9c41e7a2d5b8f3016c4e7a9b2d1f0e8c7b6a5d43";
const W = 110, H = 30;

// ------------------------------------------------------------ the files

// Each agent's files: the base version and the worktree's version. Diffs
// are computed from them, so line numbers and texts agree everywhere.
const FILES = {
  lead: [
    {
      path: "src/invoice/types.ts", status: "modified",
      old: [
        "export type Currency = string;",
        "",
        "export interface Line {",
        "  sku: string;",
        "  description: string;",
        "  qty: number;",
        "  unitPrice: number;",
        "}",
        "",
        "export interface Invoice {",
        "  id: string;",
        "  customer: string;",
        "  currency: Currency;",
        "  lines: Line[];",
        "  taxRate: number; // 0.2 for 20 %",
        "}",
      ],
      new: [
        "export type Currency = string;",
        "",
        "export interface Line {",
        "  sku: string;",
        "  description: string;",
        "  qty: number;",
        "  /** Integer cents, never a float (ADR 0007). */",
        "  unitCents: number;",
        "}",
        "",
        "export interface Invoice {",
        "  id: string;",
        "  customer: string;",
        "  currency: Currency;",
        "  lines: Line[];",
        "  /** Tax rate in basis points: 2000 is 20 %. */",
        "  taxRateBp: number;",
        "}",
      ],
    },
    {
      path: "docs/adr/0007-money-in-cents.md", status: "added", untracked: true, old: [],
      new: [
        "# 7. Money is integer cents",
        "",
        "Status: accepted, 2026-11-14 (#412)",
        "",
        "## Context",
        "",
        "Invoice A-20931 came out at 140.78 where the customer's own sum was 140.79.",
        "Totals were computed in floats and the tax was rounded once, on the sum.",
        "",
        "## Decision",
        "",
        "- Every amount is an integer number of cents, from parsing to storage.",
        "- Floats appear only when an amount is formatted for display.",
        "- Tax is rounded half up on each line, then summed: the tax office does",
        "  the same, and so does the accounting export.",
        "- Input with a third decimal is refused (422), never rounded.",
        "- Currencies are the five in config/currencies.json.",
      ],
    },
  ],
  "fix-total": [
    {
      path: "src/invoice/total.ts", status: "modified",
      old: [
        'import type { Invoice } from "./types";',
        'import { round } from "../money";',
        "",
        "export function invoiceTotal(inv: Invoice): number {",
        "  const net = inv.lines.reduce((sum, l) => sum + l.unitPrice * l.qty, 0);",
        "  const tax = round(net * inv.taxRate, 2);",
        "  return round(net + tax, 2);",
        "}",
      ],
      new: [
        'import type { Invoice } from "./types";',
        'import { roundHalfUp } from "../money";',
        "",
        "/**",
        " * The invoice's total in integer cents. Tax is rounded half up on each",
        " * line, then summed: that is how the tax office computes it (#412).",
        " */",
        "export function invoiceTotalCents(inv: Invoice): number {",
        "  let total = 0;",
        "  for (const line of inv.lines) {",
        "    const net = line.unitCents * line.qty;",
        "    const tax = roundHalfUp((net * inv.taxRateBp) / 10_000);",
        "    total += net + tax;",
        "  }",
        "  return total;",
        "}",
      ],
    },
    {
      path: "src/money.ts", status: "modified",
      old: [
        "export function round(x: number, places: number): number {",
        "  const f = 10 ** places;",
        "  return Math.round(x * f) / f;",
        "}",
        "",
        "export const format = (x: number, currency: string): string =>",
        '  new Intl.NumberFormat("en", { style: "currency", currency }).format(x);',
      ],
      new: [
        "/** Round to the nearest integer; halves go away from zero (2.5 → 3, -2.5 → -3). */",
        "export function roundHalfUp(x: number): number {",
        "  return Math.sign(x) * Math.floor(Math.abs(x) + 0.5);",
        "}",
        "",
        '/** "12.30" → 1230. Throws on a third decimal. */',
        "export function parseCents(s: string): number {",
        "  const m = /^(-?)(\\d+)(?:\\.(\\d{1,2}))?$/.exec(s.trim());",
        "  if (!m) throw new RangeError(`not an amount: ${s}`);",
        '  const cents = Number(m[2]) * 100 + Number((m[3] ?? "").padEnd(2, "0"));',
        "  return m[1] ? -cents : cents;",
        "}",
        "",
        "export const formatCents = (cents: number, currency: string): string =>",
        '  new Intl.NumberFormat("en", { style: "currency", currency }).format(cents / 100);',
      ],
    },
    {
      path: "test/total.test.ts", status: "added", old: [],
      new: [
        'import { describe, expect, it } from "vitest";',
        'import { invoiceTotalCents } from "../src/invoice/total";',
        "",
        "const inv = (taxRateBp: number, ...lines: [number, number][]) => ({",
        '  id: "t", customer: "c", currency: "EUR", taxRateBp,',
        '  lines: lines.map(([unitCents, qty], i) => ({ sku: `s${i}`, description: "", unitCents, qty })),',
        "});",
        "",
        'describe("invoiceTotalCents", () => {',
        '  it("#412: A-20931 rounds the tax of each line, then sums", () => {',
        "    expect(invoiceTotalCents(inv(2000, [1999, 3], [1234, 1], [4502, 1]))).toBe(14_079);",
        "  });",
        "",
        '  it("#412: half a cent of tax rounds up", () => {',
        "    expect(invoiceTotalCents(inv(1000, [1025, 1]))).toBe(1_128);",
        "  });",
        "",
        '  it("#412: a credit line rounds away from zero", () => {',
        "    expect(invoiceTotalCents(inv(1000, [-1025, 1]))).toBe(-1_128);",
        "  });",
        "});",
      ],
    },
  ],
  "add-validate": [
    {
      path: "src/invoice/validate.ts", status: "added", old: [],
      new: [
        'import type { Invoice } from "./types";',
        'import currencies from "../../config/currencies.json";',
        "",
        "export class InvalidInvoice extends Error {",
        "  constructor(readonly field: string, message: string) {",
        "    super(message);",
        "  }",
        "}",
        "",
        "const ALLOWED = new Set<string>(currencies);",
        "",
        "/** Throws InvalidInvoice naming the first field that is wrong. */",
        "export function validateInvoice(inv: Invoice): void {",
        "  if (!ALLOWED.has(inv.currency)) {",
        '    throw new InvalidInvoice("currency", `${inv.currency} is not one of ${[...ALLOWED].join(", ")}`);',
        "  }",
        "  inv.lines.forEach((line, i) => {",
        "    if (!Number.isInteger(line.qty) || line.qty <= 0) {",
        '      throw new InvalidInvoice(`lines[${i}].qty`, "a quantity is a whole number above zero");',
        "    }",
        "    if (!Number.isInteger(line.unitCents)) {",
        "      // Refuse rather than round: a third decimal is a mistake upstream.",
        '      throw new InvalidInvoice(`lines[${i}].unitCents`, "a unit price has at most two decimals");',
        "    }",
        "  });",
        "}",
      ],
    },
    {
      path: "src/routes/invoices.ts", status: "modified",
      old: [
        'import { Router } from "express";',
        'import { db } from "../db";',
        'import { invoiceTotal } from "../invoice/total";',
        'import type { Invoice } from "../invoice/types";',
        "",
        "export const router = Router();",
        "",
        'router.post("/invoices", async (req, res) => {',
        "  const inv = req.body as Invoice;",
        "  const saved = await db.invoices.insert({ ...inv, total: invoiceTotal(inv) });",
        "  res.status(201).json(saved);",
        "});",
      ],
      new: [
        'import { Router } from "express";',
        'import { db } from "../db";',
        'import { invoiceTotal } from "../invoice/total";',
        'import type { Invoice } from "../invoice/types";',
        'import { InvalidInvoice, validateInvoice } from "../invoice/validate";',
        "",
        "export const router = Router();",
        "",
        'router.post("/invoices", async (req, res) => {',
        "  const inv = req.body as Invoice;",
        "  try {",
        "    validateInvoice(inv);",
        "  } catch (e) {",
        "    if (e instanceof InvalidInvoice) return res.status(422).json({ error: e.message, field: e.field });",
        "    throw e;",
        "  }",
        "  const saved = await db.invoices.insert({ ...inv, total: invoiceTotal(inv) });",
        "  res.status(201).json(saved);",
        "});",
      ],
    },
    {
      path: "config/currencies.json", status: "added", old: [],
      new: ['["EUR", "GBP", "USD", "CHF", "SEK"]'],
    },
  ],
};

// ------------------------------------------------------------- the diff

function diffOps(a, b) {
  const n = a.length, m = b.length;
  const dp = Array.from({ length: n + 1 }, () => new Int32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const ops = [];
  let i = 0, j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && a[i] === b[j]) { ops.push({ kind: " ", text: a[i], old: i + 1, new: j + 1 }); i++; j++; }
    else if (i < n && (j >= m || dp[i + 1][j] >= dp[i][j + 1])) { ops.push({ kind: "-", text: a[i], old: i + 1, new: null }); i++; }
    else { ops.push({ kind: "+", text: b[j], old: null, new: j + 1 }); j++; }
  }
  return ops;
}

function hunksOf(ops, ctx = 3) {
  const changed = ops.map((o, k) => (o.kind !== " " ? k : -1)).filter((k) => k >= 0);
  const ranges = [];
  for (const k of changed) {
    const lo = Math.max(0, k - ctx), hi = Math.min(ops.length - 1, k + ctx);
    const last = ranges[ranges.length - 1];
    if (last && lo <= last[1] + 1) last[1] = Math.max(last[1], hi); else ranges.push([lo, hi]);
  }
  return ranges.map(([lo, hi]) => {
    const slice = ops.slice(lo, hi + 1);
    // The line numbers where the hunk starts: the first line it shows on each side.
    let oldStart = slice.find((o) => o.old)?.old ?? 0;
    let newStart = slice.find((o) => o.new)?.new ?? 0;
    if (!slice.some((o) => o.old)) oldStart = 0;
    if (!slice.some((o) => o.new)) newStart = 0;
    return { old_start: oldStart, new_start: newStart, lines: slice.map((o) => ({ kind: o.kind, text: o.text, new_line: o.new, old_line: o.old })) };
  });
}

function fileDiff(f) {
  return { path: f.path, status: f.status, binary: false, ...(f.untracked ? { untracked: true } : {}), hunks: hunksOf(diffOps(f.old, f.new)) };
}

// For the agents outside the story: a small change to their first file.
function genericFiles(row, preset) {
  const file = preset?.files?.[0] || "src/lib.rs";
  if (/\.md$/.test(file)) {
    return [{ path: file, status: "modified",
      old: ["## Searching", "", "```console", "$ chloedb query 'SELECT title FROM docs WHERE MATCH(body, \"posting lists\") LIMIT 3'", "title", "Posting lists", "```"],
      new: ["## Searching", "", "```console", "$ chloedb query 'SELECT title FROM docs WHERE MATCH(body, \"posting lists\") LIMIT 3'", "title", "Posting lists", "Codecs for posting lists", "Skipping inside a posting list", "```"] }];
  }
  if (/\.(js|html)$/.test(file)) {
    return [{ path: file, status: "modified",
      old: ["export function openDialog(d) {", "  d.showModal();", "}"],
      new: ["export function openDialog(d, opener = document.activeElement) {", "  d.showModal();", "  // Focus goes back where it came from when the dialog closes.", "  d.addEventListener(\"close\", () => opener?.focus?.(), { once: true });", "}"] }];
  }
  return [{ path: file, status: "modified",
    old: ["fn read_string(&mut self) -> Result<Token> {", "    let start = self.pos;", "    while let Some(c) = self.bump() {", "        if c == '\\'' {", "            break;", "        }", "    }", "    Ok(Token::Str(self.src[start..self.pos - 1].to_string()))", "}"],
    new: ["fn read_string(&mut self) -> Result<Token> {", "    let start = self.pos;", "    while let Some(c) = self.bump() {", "        if c == '\\'' {", "            return Ok(Token::Str(self.src[start..self.pos - 1].to_string()));", "        }", "    }", "    // The input ended inside the literal: say where it started.", "    Err(Error::UnterminatedString { at: start - 1 })", "}"] }];
}

// --------------------------------------------------------- the recording

class Cast {
  constructor() { this.ev = []; this.t = 0; this.col = 0; }
  at(ms) { this.t = Math.max(this.t, Math.round(ms)); return this; }
  o(s) { this.ev.push([this.t, "o", s.replace(/\r?\n/g, "\r\n")]); }
  wait(ms) { this.t += Math.round(ms); }
  think(ms, label = "Pondering") {
    const frames = "·✢✳✶✻✽✻✶✳✢";
    const t0 = this.t, end = this.t + ms;
    let k = 0;
    while (this.t < end) {
      const secs = Math.floor((this.t - t0) / 1000);
      const tok = (0.4 + (this.t - t0) / 2600).toFixed(1);
      this.o(`\r${E}2K${orange(frames[k % frames.length])} ${orange(`${label}…`)} ${gray(`(${secs}s · ↑ ${tok}k tokens · esc to interrupt)`)}`);
      k++;
      this.t += 120;
    }
    this.t = end;
    this.o(`\r${E}2K`);
  }
  say(text) {
    this.o(`${orange("●")} `);
    let col = 2;
    for (const w of text.split(" ")) {
      if (col + w.length + 1 > W - 6) { this.o("\n  "); col = 2; }
      this.o(`${w} `);
      col += w.length + 1;
      this.t += 28 + w.length * 7;
    }
    this.o("\n\n");
    this.t += 180;
  }
  tool(name, arg, result, ms = 900) {
    this.o(`${orange("●")} ${bold(name)}(${arg})\n`);
    this.think(ms, "Running");
    this.o(`  ${gray("⎿")}  ${result.startsWith("\x1b") ? result : gray(result)}\n\n`);
  }
  prompt(text) {
    const lines = wrap(text, W - 6);
    this.o(`${gray(">")} ${lines.join("\n  ")}\n\n`);
  }
  inputBox(model) {
    this.o(box([`${gray(">")} `], 80));
    this.o(gray(`  ? for shortcuts${" ".repeat(46)}${model ? `model: ${model}` : ""}\n`));
  }
}

function wrap(text, w) {
  const out = [];
  let line = "";
  for (const word of text.split(" ")) {
    if (line && line.length + word.length + 1 > w) { out.push(line); line = word; } else line = line ? `${line} ${word}` : word;
  }
  if (line) out.push(line);
  return out;
}

// The lines Claude Code shows under an Edit or a Write: the final diff's
// lines around [from, to] of the new file.
function editBlock(file, range, verb, asWas = {}) {
  const ops = diffOps(file.old, file.new);
  if (verb === "Write") {
    const shown = file.new.slice(range[0] - 1, Math.min(range[1], range[0] + 11));
    return [
      `  ${gray("⎿")}  Wrote ${file.new.length} lines to ${bold(file.path)}`,
      ...shown.map((t, i) => `     ${gray(String(range[0] + i).padStart(4))} ${t}`),
      ...(range[1] - range[0] + 1 > shown.length ? [`     ${gray(`… +${range[1] - range[0] + 1 - shown.length} lines`)}`] : []),
    ].join("\n") + "\n\n";
  }
  const lo = ops.findIndex((o) => o.new != null && o.new >= range[0] - 1);
  let hi = ops.length - 1;
  for (let k = ops.length - 1; k >= 0; k--) if (ops[k].new != null && ops[k].new <= range[1] + 1) { hi = k; break; }
  let from = lo;
  while (from > 0 && ops[from - 1].kind === "-") from--;
  const slice = ops.slice(Math.max(0, from), hi + 1);
  const adds = slice.filter((o) => o.kind === "+").length, dels = slice.filter((o) => o.kind === "-").length;
  const rows = slice.map((o) => {
    const n = String(o.kind === "-" ? o.old : o.new).padStart(4);
    if (o.kind === "+") return `     ${gray(n)} ${green(`+ ${asWas[o.new] ?? o.text}`)}`;
    if (o.kind === "-") return `     ${gray(n)} ${red(`- ${o.text}`)}`;
    return `     ${gray(n)}   ${o.text}`;
  });
  return [`  ${gray("⎿")}  Updated ${bold(file.path)} with ${adds} addition${adds === 1 ? "" : "s"} and ${dels} removal${dels === 1 ? "" : "s"}`, ...rows].join("\n") + "\n\n";
}

// -------------------------------------------------------------- the story

export function buildStory(ctx) {
  const { agents, makeAgent, addEvent, agentJson, now } = ctx;
  const T0 = now() - 40 * 60_000;
  const at = (min, sec = 0) => T0 + Math.round((min * 60 + sec) * 1000);
  const iso = (ms) => new Date(ms).toISOString();

  const SESS = {
    lead: "5be0c7d2-4f1a-4b8e-9a51-0d2c3e6f7a18",
    "fix-total": "1c2e7f40-93ab-4d0e-8c11-5a6b7c8d9e01",
    "add-validate": "88aa31d9-2c4d-4e5f-a6b7-c8d9e0f1a2b3",
    older: "e41b0a6c-7d8e-4f90-a1b2-c3d4e5f60718",
    oldest: "73f9d2e5-1a2b-4c3d-8e4f-5a6b7c8d9e0f",
  };
  const rid = (who, n) => `${SESS[who].slice(0, 8)}:${n}`;

  const TASKS = {
    lead: "Invoices come out a cent off on some orders (#412). Find out why and fix it, and make invoice input strict while you are at it. Split the work between two agents if that helps, and review what they did before you finish.",
    "fix-total": "In src/invoice/total.ts and src/money.ts, compute invoice totals in integer cents and round the tax half up on each line, then sum: that is how the tax office computes it (#412). Add tests for the three orders in #412. Do not touch validation: add-validate owns src/invoice/validate.ts and the route.",
    "add-validate": "Add validateInvoice() in src/invoice/validate.ts and call it from POST /invoices: reject quantities that are not whole and above zero, unit prices with more than two decimals, and currencies not in config/currencies.json. Answer 422 with the field that failed. Do not touch total.ts or money.ts: fix-total owns them.",
  };
  const FOLLOWUPS = {
    "fix-total": { at: at(13, 0), text: "Also rename invoiceTotal so its name says it returns cents. Change its callers in your own files only; I will fix the route after both of you are done.", by: "lead" },
    "add-validate": { at: at(13, 30), text: "Do not round unit prices with a third decimal: refuse them with 422. A third decimal is a bug upstream, and rounding it hides the bug.", by: "lead" },
    lead: { at: at(24, 0), text: "Good. Write the money rules down as an ADR in docs/adr, so the next session finds them in the repo and not only in memory.", by: "you" },
  };

  // The decisions ChloeCtx distilled, as `memory` rows of kind decision.
  const DEC = [
    { id: rid("oldest", 44), who: null, sess: "oldest", ts: T0 - 7 * 86_400_000 - 3_600_000 * 3, text: "Compute invoice totals in floating point and round to two places once, on the sum.", files: ["src/invoice/total.ts"], superseded_by: rid("lead", 12) },
    { id: rid("older", 19), who: null, sess: "older", ts: T0 - 2 * 86_400_000 + 3_600_000 * 2.2, text: "Round an invoice's tax half-even (banker's rounding) on the invoice total, to match the accounting export.", files: ["src/invoice/total.ts", "src/export/accounting.ts"], superseded_by: rid("fix-total", 31) },
    { id: rid("older", 27), who: null, sess: "older", ts: T0 - 2 * 86_400_000 + 3_600_000 * 2.5, text: "Use Intl.NumberFormat for display only; never parse its output back into an amount.", files: ["src/money.ts"] },
    { id: rid("lead", 12), who: "lead", ts: at(3, 0), text: "Keep every amount in integer cents end to end; convert to a decimal only when formatting for display.", files: ["src/invoice/types.ts", "src/money.ts"] },
    { id: rid("lead", 15), who: "lead", ts: at(4, 30), text: "Split #412 in two: fix-total owns total.ts and money.ts, add-validate owns validate.ts and the route; neither edits the other's files.", files: [] },
    { id: rid("add-validate", 14), who: "add-validate", ts: at(8, 20), text: "Round unit prices with more than two decimals to the nearest cent on input.", files: ["src/invoice/validate.ts"], superseded_by: rid("add-validate", 22) },
    { id: rid("fix-total", 27), who: "fix-total", ts: at(9, 10), text: "Halves round away from zero, so a credit note mirrors the invoice it cancels.", files: ["src/money.ts"] },
    { id: rid("add-validate", 25), who: "add-validate", ts: at(9, 20), text: "Allowed currencies come from config/currencies.json, not all of ISO 4217: the payment provider settles only five.", files: ["config/currencies.json", "src/invoice/validate.ts"] },
    { id: rid("add-validate", 29), who: "add-validate", ts: at(10, 50), text: "Answer an invalid invoice with 422 and the name of the first field that failed.", files: ["src/routes/invoices.ts"] },
    { id: rid("fix-total", 31), who: "fix-total", ts: at(12, 5), text: "Round tax half up on each line, then sum: the tax office rounds per line, and Finance confirmed in #412 that the accounting export does too.", files: ["src/invoice/total.ts", "src/money.ts"] },
    { id: rid("add-validate", 22), who: "add-validate", ts: at(14, 30), text: "Refuse unit prices with more than two decimals instead of rounding them: a third decimal means a mistake upstream.", files: ["src/invoice/validate.ts"] },
    { id: rid("fix-total", 36), who: "fix-total", ts: at(15, 10), text: "Rename invoiceTotal to invoiceTotalCents, so no caller can mistake the unit.", files: ["src/invoice/total.ts", "test/total.test.ts"] },
    { id: rid("lead", 40), who: "lead", ts: at(25, 40), text: "Record the money rules as ADR 0007, so later sessions find them in the repo and not only in memory.", files: ["docs/adr/0007-money-in-cents.md"] },
  ];
  const dec = (id) => DEC.find((d) => d.id === id);
  const OTHER_NOTES = {
    factTax: { kind: "fact", text: "The tax office's calculator rounds each line's tax half up; Finance confirmed in #412 that the accounting export does the same.", ts: iso(at(12, 5)) },
    prefTests: { kind: "preference", text: "Tests name the issue they cover: it(\"#412: …\").", ts: iso(at(15, 10)) },
    factDisjoint: { kind: "fact", text: "fix-total and add-validate touched no file in common; the route still calls invoiceTotal and needs the rename after both merge.", ts: iso(at(21, 0)) },
  };
  const note = (d) => ({ kind: "decision", id: d.id, text: d.text, ts: new Date(d.ts).toISOString() });
  const WEB = { kind: "tool_result", id: rid("fix-total", 18), text: "WebFetch stackoverflow.com: Math.round(1.005 * 100) / 100 gives 1 in JavaScript, not 1.01; use Number.EPSILON or work in integers.", score: 0.52, decision: "excerpt", untrusted: true };
  const knewOf = (d, score, decision = "knowledge") => ({ kind: "decision", id: d.id, text: d.text, score, decision });

  // The edits: which lines of which file, in which turn, and what the card says.
  const EDITS = [
    { who: "lead", path: "src/invoice/types.ts", lines: [7, 8], extra: [16, 17], turn: 1, t: at(2, 10), verb: "Update",
      say: "The types carry floats: unitPrice and taxRate. Integer cents first, so nothing downstream can drift.",
      knew: [knewOf(dec(rid("oldest", 44)), 0.71), knewOf(dec(rid("older", 19)), 0.66), knewOf(dec(rid("older", 27)), 0.48, "take")],
      why: () => [note(dec(rid("lead", 12))), note(dec(rid("lead", 15))), OTHER_NOTES.factDisjoint] },
    { who: "fix-total", path: "src/money.ts", lines: [1, 15], turn: 1, t: at(8, 40), verb: "Update",
      say: "money.ts rounds with Math.round on floats. I'll replace it with an integer half-up rounding and a parser that refuses a third decimal.",
      knew: [knewOf(dec(rid("lead", 12)), 0.79), knewOf(dec(rid("older", 27)), 0.61, "take"), WEB],
      why: () => [note(dec(rid("fix-total", 27))), note(dec(rid("fix-total", 31))), OTHER_NOTES.factTax] },
    { who: "fix-total", path: "src/invoice/total.ts", lines: [2, 7], extra: [9, 15], turn: 1, t: at(11, 41), verb: "Update", star: true, asWas: { 8: "export function invoiceTotal(inv: Invoice): number {" },
      knew: [knewOf(dec(rid("older", 19)), 0.81), knewOf(dec(rid("lead", 12)), 0.74), WEB, knewOf(dec(rid("older", 27)), 0.44, "listed")],
      why: () => [note(dec(rid("fix-total", 27))), note(dec(rid("fix-total", 31))), OTHER_NOTES.factTax] },
    { who: "fix-total", path: "src/invoice/total.ts", lines: [8, 8], turn: 2, t: at(13, 40), verb: "Update", prompt: "fix-total",
      say: "Renaming to invoiceTotalCents; the only caller in my files is the test I'm about to write.",
      knew: [knewOf(dec(rid("fix-total", 31)), 0.83), knewOf(dec(rid("lead", 15)), 0.58, "take")],
      why: () => [note(dec(rid("fix-total", 36))), OTHER_NOTES.prefTests] },
    { who: "fix-total", path: "test/total.test.ts", lines: [1, 21], turn: 2, t: at(14, 50), verb: "Write", prompt: "fix-total",
      say: "Now the three orders from #412 as tests, named after the issue.",
      knew: [knewOf(dec(rid("fix-total", 31)), 0.77), knewOf(dec(rid("fix-total", 27)), 0.6, "take")],
      why: () => [note(dec(rid("fix-total", 36))), OTHER_NOTES.prefTests] },
    { who: "add-validate", path: "src/invoice/validate.ts", lines: [1, 20], extra: [25, 26], turn: 1, t: at(8, 0), verb: "Write", commit: "b7e2f19c4a",
      say: "There is no validation yet. I'll write validateInvoice with one error class that names the failing field.",
      knew: [knewOf(dec(rid("lead", 12)), 0.7), knewOf(dec(rid("lead", 15)), 0.64, "take")],
      why: () => [note(dec(rid("add-validate", 14))), note(dec(rid("add-validate", 25))), note(dec(rid("add-validate", 29)))] },
    { who: "add-validate", path: "src/routes/invoices.ts", lines: [5, 5], extra: [11, 16], turn: 1, t: at(10, 30), verb: "Update", commit: "b7e2f19c4a",
      say: "Wiring it into POST /invoices: an InvalidInvoice becomes a 422 with the field.",
      knew: [knewOf(dec(rid("lead", 15)), 0.62, "take")],
      why: () => [note(dec(rid("add-validate", 14))), note(dec(rid("add-validate", 25))), note(dec(rid("add-validate", 29)))] },
    { who: "add-validate", path: "src/invoice/validate.ts", lines: [21, 24], turn: 2, t: at(14, 10), verb: "Update", prompt: "add-validate",
      say: "lead is right: rounding a third decimal hides the bug upstream. Refusing it instead.",
      knew: [knewOf(dec(rid("add-validate", 14)), 0.86), knewOf(dec(rid("lead", 12)), 0.69)],
      why: () => [note(dec(rid("add-validate", 22)))] },
    { who: "lead", path: "docs/adr/0007-money-in-cents.md", lines: [1, 17], turn: 2, t: at(25, 10), verb: "Write", prompt: "lead",
      say: "Writing ADR 0007 from the decisions of this session and both workers'.",
      knew: [knewOf(dec(rid("lead", 12)), 0.88), knewOf(dec(rid("fix-total", 31)), 0.84), knewOf(dec(rid("add-validate", 22)), 0.8), knewOf(dec(rid("add-validate", 25)), 0.73, "take"), knewOf(dec(rid("add-validate", 14)), 0.41, "listed")],
      why: () => [note(dec(rid("lead", 40)))] },
  ];
  const COMMITS = {
    b7e2f19c4a: { sha: "b7e2f19c4a8d2e6f1a3b5c7d9e0f2a4b6c8d0e1f", date: new Date(at(11, 5)).toISOString(), subject: "Validate invoices on POST /invoices (#412)" },
  };

  // ----------------------------------------------------- the agents' rows
  const story = {};
  const mk = (name, extra) => {
    const a = makeAgent({
      name, repo: STORY_REPO, cwd: `/home/user/.local/share/chloedb-workbench/worktrees/invoicer-3f1c9a0e/${name}`, branch: `wb/${name}`,
      task: TASKS[name], session_id: SESS[name], base: BASE, model: extra.model, ...extra,
    }, { files: ["src/invoice/total.ts", "src/money.ts", "test/total.test.ts"], grep: "roundHalfUp", cmd: "npx vitest run", n: 0 });
    story[name] = a;
    return a;
  };
  const lead = mk("lead", { created: at(0), state: "idle", state_since: at(26, 0), model: "opus", parent: null, depth: 0, pid: 48211 });
  const fixTotal = mk("fix-total", { created: at(5, 0), state: "exited", state_since: at(16, 40), ended: at(16, 40), exit_code: 0, model: "sonnet", parent: lead.row.id, depth: 1, pid: 48377 });
  const addValidate = mk("add-validate", { created: at(5, 20), state: "exited", state_since: at(20, 0), ended: at(20, 0), exit_code: 0, model: "sonnet", parent: lead.row.id, depth: 1, pid: 48390 });
  for (const a of Object.values(story)) a.row.started_ms = a.row.created;

  // ------------------------------------------ scripts: casts and events
  const rel = (a, abs) => abs - a.row.created;
  const fileOf = (who, path) => FILES[who].find((f) => f.path === path);

  function play(a, steps) {
    const c = new Cast();
    const ev = (kind, tool, summary, abs) => addEvent(a, kind, tool, summary, abs);
    ev("spawned", null, `Spawned${a.row.parent ? ` by ${agents.get(a.row.parent).row.name}` : ""} in ${a.row.cwd}`, a.row.created);
    c.o("\x1b[2J\x1b[H");
    c.o(box([`${orange("✻")} ${bold("Welcome to Claude Code!")}`, "", gray("  /help for help, /status for your current setup"), "", gray(`  cwd: ${a.row.cwd}`)], 92, orange));
    c.o("\n");
    ev("SessionStart", null, "session started (startup)", a.row.created + 900);
    for (const s of [...steps].sort((x, y) => x.t - y.t)) {
      c.at(rel(a, s.t));
      if (s.prompt) {
        if (s.by && s.by !== "task") ev("input", null, `typed: ${s.prompt.slice(0, 80)}`, s.t - 200);
        c.prompt(s.prompt);
        ev("UserPromptSubmit", null, `prompt: ${s.prompt.slice(0, 200)}`, s.t);
      } else if (s.think) c.think(s.think, s.label);
      else if (s.say) c.say(s.say);
      else if (s.tool) {
        ev("PreToolUse", s.tool, `${s.tool}: ${s.arg}`, a.row.created + c.t);
        c.tool(s.tool, s.arg, s.result, s.ms || 900);
        ev("PostToolUse", s.tool, `${s.tool} done: ${s.result.replace(/\x1b\[[0-9;]*m/g, "")}`, a.row.created + c.t);
      } else if (s.edit) {
        const e = s.edit;
        const f = fileOf(e.who, e.path);
        const tEdit = rel(a, e.t);
        c.o(`${orange("●")} ${bold(e.verb)}(${e.path})\n`);
        ev("PreToolUse", e.verb === "Write" ? "Write" : "Edit", `${e.verb === "Write" ? "Write" : "Edit"}: ${e.path}`, a.row.created + c.t);
        c.think(Math.max(300, tEdit - c.t), "Editing");
        c.at(tEdit);
        c.o(editBlock(f, [e.lines[0], (e.extra || e.lines)[1]], e.verb, e.asWas));
        ev("PostToolUse", e.verb === "Write" ? "Write" : "Edit", `${e.verb === "Write" ? "Write" : "Edit"} done: ${e.path}`, e.t);
      } else if (s.stop) {
        c.say(s.stop);
        c.inputBox(a.row.model);
        ev("Stop", null, "turn finished", s.t + 400);
      } else if (s.exit != null) {
        c.o(`\n${gray(`[process exited with code ${s.exit}]`)}\n`);
        ev("SessionEnd", null, "session ended (prompt_input_exit)", s.t);
        ev("exited", null, `Exited with code ${s.exit}`, s.t + 300);
      }
    }
    a.cast = { width: W, height: H, events: c.ev };
    a.buf = Buffer.from(c.ev.filter((x) => x[1] === "o").map((x) => x[2]).join(""));
  }

  // The 20 s before an edit: generic, or the star edit's own.
  function lead_in(e) {
    const t = (s) => e.t + s * 1000;
    if (e.star) {
      return [
        { t: t(-21), tool: "Read", arg: "src/invoice/types.ts", result: "Read 18 lines", ms: 700 },
        { t: t(-19.6), think: 3600 },
        { t: t(-15.8), say: "The total rounds once, on the sum, so the half cents in each line's tax are lost together: A-20931's lines carry 1199.4, 246.8 and 900.4 cents of tax." },
        { t: t(-10.4), tool: "chloectx - memory_search (MCP)", arg: 'query: "rounding tax invoice total"', result: `3 records · ${blue("decision")}: Round an invoice's tax half-even on the invoice total (2 days ago)`, ms: 1100 },
        { t: t(-8.2), say: "Memory says half-even on the total, but that is the rule #412 is about: Finance confirmed the tax office rounds each line, half up. I'll follow the issue and round per line." },
        { t: t(-1.4), edit: e },
        { t: t(1.1), say: "Now the three orders from #412 as tests, and the rename lead may ask for can wait." },
        { t: t(5), tool: "Bash", arg: "npx vitest run test/total.test.ts", result: green("✓ 3 passed (3)"), ms: 2400 },
      ];
    }
    return [
      { t: t(-19), think: 2600 },
      { t: t(-16), say: e.say },
      { t: t(-9.5), tool: e.verb === "Write" ? "Glob" : "Read", arg: e.verb === "Write" ? `pattern: "${e.path.replace(/[^/]+$/, "*")}"` : e.path, result: e.verb === "Write" ? "Found 2 files" : `Read ${fileOf(e.who, e.path).old.length} lines`, ms: 800 },
      { t: t(-7.4), think: 5400 },
      { t: t(-1.4), edit: e },
    ];
  }

  const edits = (who) => EDITS.filter((e) => e.who === who).sort((x, y) => x.t - y.t);
  const early = (a, files) => {
    const t = (s) => a.row.created + s * 1000;
    return [
      { t: t(1.5), prompt: a.row.task, by: "task" },
      { t: t(3), think: 2800 },
      { t: t(6.2), say: "I'll start with the files the task names." },
      ...files.map((p, i) => ({ t: t(9 + i * 4), tool: "Read", arg: p, result: `Read ${fileOf(a.row.name, p)?.old.length || 12} lines`, ms: 900 })),
    ];
  };
  function stepsFor(who, files, middle) {
    const a = story[who];
    const list = [...early(a, files)];
    const fu = FOLLOWUPS[who];
    for (const e of edits(who)) {
      if (fu && e.t > fu.at && !list.includes(fu.step)) {
        fu.step = { t: fu.at, prompt: fu.text, by: fu.by };
        list.push(...(middle?.(fu.at) || []), fu.step);
      }
      list.push(...lead_in(e));
    }
    return list;
  }

  play(fixTotal, [
    ...stepsFor("fix-total", ["src/invoice/total.ts", "src/money.ts"], (t) => [{ t: t - 30_000, stop: "Totals are in integer cents and the tax is rounded half up per line. 3 files changed; tests next once lead has looked." }]),
    { t: at(15, 40), tool: "Bash", arg: "npx vitest run", result: green("✓ 14 passed (14)"), ms: 2600 },
    { t: at(16, 0), tool: "Bash", arg: 'git commit -am "Totals in integer cents; tax rounded half up per line (#412)"', result: "[wb/fix-total 3f9a2c1] Totals in integer cents; tax rounded half up per line (#412)", ms: 700 },
    { t: at(16, 10), stop: "Done: invoiceTotalCents, tax half up per line, three #412 tests. All 14 tests pass." },
    { t: at(16, 40), exit: 0 },
  ]);
  play(addValidate, [
    ...stepsFor("add-validate", ["src/routes/invoices.ts", "src/invoice/types.ts"], () => [
      { t: at(9, 0), tool: "Bash", arg: `echo '["EUR", "GBP", "USD", "CHF", "SEK"]' > config/currencies.json`, result: "(no output)", ms: 500 },
      { t: at(11, 4), tool: "Bash", arg: 'git add -A && git commit -m "Validate invoices on POST /invoices (#412)"', result: "[wb/add-validate b7e2f19] Validate invoices on POST /invoices (#412)", ms: 800 },
      { t: at(11, 15), stop: "validateInvoice is in place and wired into POST /invoices; invalid input answers 422 with the field." },
    ]),
    { t: at(15, 0), stop: "A third decimal is now refused with 422 instead of rounded." },
    { t: at(20, 0), exit: 0 },
  ]);
  play(lead, [
    { t: at(0, 1.5), prompt: TASKS.lead, by: "task" },
    { t: at(0, 4), think: 3000 },
    { t: at(0, 8), say: "I'll find where totals are computed and how amounts are typed." },
    { t: at(0, 12), tool: "Grep", arg: 'pattern: "taxRate|unitPrice"', result: "Found 4 files", ms: 900 },
    { t: at(0, 16), tool: "Read", arg: "src/invoice/total.ts", result: "Read 8 lines", ms: 700 },
    ...lead_in(EDITS[0]),
    { t: at(3, 10), say: "The cause: the tax is rounded once on a float sum. I'll split the fix: fix-total takes totals and rounding, add-validate takes input validation." },
    { t: at(5, 0), tool: "workbench - spawn_agent (MCP)", arg: 'name: "fix-total", task: "In src/invoice/total.ts and src/money.ts, compute invoice totals in…"', result: "Started fix-total on wb/fix-total", ms: 600 },
    { t: at(5, 20), tool: "workbench - spawn_agent (MCP)", arg: 'name: "add-validate", task: "Add validateInvoice() in src/invoice/validate.ts and call it from…"', result: "Started add-validate on wb/add-validate", ms: 600 },
    { t: at(5, 30), tool: "workbench - wait_agent (MCP)", arg: 'id: "fix-total", until: "idle"', result: "fix-total is idle: Totals are in integer cents and the tax is rounded half up per line…", ms: 6000 },
    { t: at(13, 0), tool: "workbench - send_to_agent (MCP)", arg: `id: "fix-total", text: "${FOLLOWUPS["fix-total"].text.slice(0, 60)}…"`, result: "Sent", ms: 400 },
    { t: at(13, 30), tool: "workbench - send_to_agent (MCP)", arg: `id: "add-validate", text: "${FOLLOWUPS["add-validate"].text.slice(0, 60)}…"`, result: "Sent", ms: 400 },
    { t: at(16, 50), tool: "Bash", arg: "git -C ../fix-total diff --stat 9c41e7a && git -C ../add-validate diff --stat 9c41e7a", result: "6 files changed, 71 insertions(+), 11 deletions(-)", ms: 1200 },
    { t: at(21, 0), stop: "Both workers are done. fix-total: totals in integer cents, tax half up per line, 3 tests for #412. add-validate: validateInvoice and 422s. The route still calls invoiceTotal: rename it when you merge." },
    { t: at(24, 0), prompt: FOLLOWUPS.lead.text, by: "you" },
    ...lead_in(EDITS[EDITS.length - 1]),
    { t: at(26, 0), stop: "ADR 0007 is written in docs/adr. It records the five money rules, each with the decision it came from." },
  ]);

  // --------------------------------------------------------- the other repo
  // A few decisions in /home/user/chloedb too, from the agents started there.
  function chloedbDecisions() {
    const by = (n) => [...agents.values()].find((a) => a.row.name === n && a.row.repo !== STORY_REPO);
    const list = [];
    const push = (a, n, minsAgo, text, files, superseded_by = null) => {
      const s = a ? a.row.session_id : "2b9c0d1e-0000-4000-8000-000000000000";
      list.push({ id: `${s.slice(0, 8)}:${n}`, ts: now() - minsAgo * 60_000, session: s, agent: a ? agentJson(a) : null, text, files, superseded_by });
    };
    push(null, 61, 3 * 1440, "Posting-list codecs are chosen per segment, never per term.", ["chloedb/src/postings/mod.rs"]);
    push(null, 70, 3 * 1440 - 20, "Benchmark codecs on MS MARCO passages.", ["chloedb/benches/postings.rs"], by("bench-postings") ? `${by("bench-postings").row.session_id.slice(0, 8)}:9` : null);
    if (by("bench-postings")) push(by("bench-postings"), 9, 8, "Benchmark on SciFact only for now: it is the smallest BEIR collection the relevance gate already fetches.", ["chloedb/benches/postings.rs"]);
    if (by("bench-gate")) push(by("bench-gate"), 5, 2, "Report nDCG@10 next to the gate's committed number rather than as a new baseline.", ["chloedb/tests/relevance_gate.rs"]);
    if (by("fix-lexer")) push(by("fix-lexer"), 11, 4, "A string literal that ends at the end of the input is an error that names where it started, not a token missing its last character.", ["chloedb/src/sql/lexer.rs"]);
    return list.sort((x, y) => y.ts - x.ts);
  }

  // ------------------------------------------------------------- the routes
  const nameOfAgent = (a) => a.row.name;
  const lineage = (a) => {
    const out = [];
    let p = a.row.parent && agents.get(a.row.parent);
    while (p) { out.push(agentJson(p)); p = p.row.parent && agents.get(p.row.parent); }
    return out;
  };
  const filesOf = (a) => (story[a.row.name] === a ? FILES[a.row.name] : (a.genericFiles ||= genericFiles(a.row, a.preset)));

  function diff(a) {
    return { base: a.row.base, files: filesOf(a).map(fileDiff) };
  }

  function recording(a, from, to) {
    if (!a.cast) return null;
    const evs = a.cast.events;
    const end = evs.length ? evs[evs.length - 1][0] : 0;
    from = Math.max(0, from ?? 0);
    to = Math.min(to ?? end, end + 1);
    const before = evs.filter((x) => x[0] < from && x[1] === "o").map((x) => x[2]).join("");
    return { width: a.cast.width, height: a.cast.height, started_ms: a.row.started_ms, before: before.slice(-256 * 1024), events: evs.filter((x) => x[0] >= from && x[0] <= to), truncated: false, duration_ms: end };
  }

  function editFor(who, path, n) {
    return EDITS.filter((e) => e.who === who && e.path === path && ((n >= e.lines[0] && n <= e.lines[1]) || (e.extra && n >= e.extra[0] && n <= e.extra[1]))).sort((x, y) => y.t - x.t)[0];
  }

  function why(a, path, n) {
    const f = filesOf(a).find((x) => x.path === path);
    if (!f) return null;
    const text = f.new[n - 1];
    if (text == null) return null;
    const ops = diffOps(f.old, f.new);
    const op = ops.find((o) => o.new === n);
    const inStory = story[a.row.name] === a;
    const card = {
      line: { path, line: n, text },
      where: { repo: a.row.repo, worktree: a.row.cwd, branch: a.row.branch, commit: null },
      who: { agent: agentJson(a), lineage: lineage(a), prompt: null },
      when: null, knew: [], why: [], worth_a_look: [], rewind: null, found: "workbench", missing: [],
    };
    if (op?.kind !== "+") {
      card.who = { agent: null, lineage: [], prompt: null };
      card.where.commit = { sha: BASE, date: new Date(T0 - 5 * 86_400_000).toISOString(), subject: "Invoices: export to accounting" };
      card.found = "git";
      card.missing = [`${a.row.name} did not write this line: it is unchanged since ${a.row.base?.slice(0, 7) || "the base"}.`];
      return card;
    }
    const e = inStory && editFor(a.row.name, path, n);
    if (!e) {
      const p = { text: a.row.task || "", ts: new Date(a.row.created + 1500).toISOString(), by: a.row.parent ? agents.get(a.row.parent)?.row.name : "you" };
      card.who.prompt = a.row.task ? p : null;
      if (inStory && path === "config/currencies.json") {
        card.where.commit = COMMITS.b7e2f19c4a;
        card.found = "git";
        card.missing = [
          "ChloeCtx has no record of an edit that wrote this line (it was written outside a recorded session, or by hand).",
          "Workbench recorded no tool call of add-validate that edited config/currencies.json.",
        ];
        card.found = "workbench";
      } else {
        card.missing = [
          "The line is not committed yet, so it has no commit.",
          "ChloeCtx has no record of an edit that wrote this line (it was written outside a recorded session, or by hand).",
          `There is no terminal recording of ${a.row.name}.`,
        ];
      }
      return card;
    }
    const fu = FOLLOWUPS[e.who];
    const promptBy = e.prompt ? fu : null;
    card.who.prompt = promptBy
      ? { text: promptBy.text, ts: new Date(promptBy.at).toISOString(), by: promptBy.by }
      : { text: a.row.task, ts: new Date(a.row.created + 1500).toISOString(), by: a.row.parent ? agents.get(a.row.parent).row.name : "you" };
    card.when = { edit_ts: new Date(e.t).toISOString(), turn: e.turn, session: a.row.session_id, how: "the newest recorded edit whose new text holds the line" };
    if (e.commit) card.where.commit = COMMITS[e.commit];
    else if (e.who === "fix-total") card.where.commit = { sha: "3f9a2c1b0e7d4c2a9f8e1d3c5b7a9e0f2d4c6b8a", date: new Date(at(16, 0)).toISOString(), subject: "Totals in integer cents; tax rounded half up per line (#412)" };
    card.knew = e.knew.map((k) => {
      const d = k.id && dec(k.id);
      const now = d?.superseded_by ? `superseded by ${d.superseded_by}` : "current";
      return { kind: k.kind, id: k.id, text: k.text, score: k.score, decision: k.decision, now, untrusted: !!k.untrusted, receipt: `rc-${(k.id || "x").replace(/\W/g, "").slice(0, 6)}${e.turn}`, turn: e.turn };
    });
    card.why = e.why();
    const worth = [];
    for (const k of card.knew) {
      if (k.now.startsWith("superseded")) worth.push(`${k.id} (${k.kind}) was in context before the edit and is ${k.now.replace("superseded by", "superseded since, by")}`);
      if (k.untrusted) worth.push(`${k.id} (${k.kind}) was in context before the edit and is untrusted source: the web`);
    }
    if (card.knew.some((k) => k.now.startsWith("superseded"))) worth.push("the same request today recalls different records");
    card.worth_a_look = worth;
    const start = a.row.started_ms;
    const tool = e.t - start;
    card.rewind = { agent: a.row.id, from_ms: Math.max(0, tool - 20_000), to_ms: tool + 3_000 };
    card.rewind.edit_ms = tool;
    card.found = "chloectx";
    card.missing = card.where.commit ? [] : ["The line is not committed yet, so it has no commit."];
    return card;
  }

  function decisions(repo, limit = 200) {
    if (repo === STORY_REPO) {
      return DEC.map((d) => ({
        id: d.id, text: d.text, ts: new Date(d.ts).toISOString(), session: d.who ? SESS[d.who] : SESS[d.sess],
        agent: d.who ? agentJson(story[d.who]) : null, superseded_by: d.superseded_by || null, superseded: !!d.superseded_by, files: d.files,
      })).sort((x, y) => Date.parse(y.ts) - Date.parse(x.ts)).slice(0, limit);
    }
    if (repo === "/home/user/chloedb") return chloedbDecisions().map((d) => ({ ...d, superseded: !!d.superseded_by, ts: new Date(d.ts).toISOString() })).slice(0, limit);
    return [];
  }

  // `chloectx ask --json` prints {text, ids, ms}; the server adds the agents
  // the records' sessions map to.
  function ask(repo, question) {
    const words = new Set(question.toLowerCase().match(/[a-z0-9]+/g) || []);
    const stem = (w) => w.replace(/(ing|ed|s)$/, "");
    const qs = new Set([...words].filter((w) => w.length > 2).map(stem));
    const list = decisions(repo, 200);
    const scored = list.map((d) => {
      const ws = (d.text.toLowerCase().match(/[a-z0-9]+/g) || []).map(stem);
      return [ws.filter((w) => qs.has(w)).length + (d.superseded_by ? 0 : 0.2), d];
    }).filter(([s]) => s >= 1).sort((x, y) => y[0] - x[0]).slice(0, 5);
    if (!scored.length) return { text: "ChloeCtx found nothing relevant in memory.", ids: [], ms: 23, agents: {}, missing: [] };
    const day = (ts) => ts.slice(0, 10);
    const lines = scored.map(([, d]) => `- ${day(d.ts)} decision: ${d.text}${d.superseded_by ? ` (changed ${day(list.find((x) => x.id === d.superseded_by)?.ts || d.ts)}; superseded by ${d.superseded_by})` : ""}`);
    const top = scored[0][1];
    const excerpt = top.session === SESS["fix-total"]
      ? `[${new Date(at(11, 33)).toISOString().slice(0, 16).replace("T", " ")} · session ${SESS["fix-total"]}] assistant: Memory says half-even on the total, but that is the rule #412 is about: Finance confirmed the tax office rounds each line, half up.`
      : null;
    const text = `Notes, newest last:\n${lines.reverse().join("\n")}${excerpt ? `\n\nFrom the sessions:\n${excerpt}` : ""}\n`;
    // The agent of each record whose session is one of Workbench's.
    const ags = {};
    for (const [, d] of scored) {
      const a = [...agents.values()].find((x) => x.row.session_id === d.session);
      if (a) ags[d.id] = agentJson(a);
    }
    return { text, ids: scored.map(([, d]) => d.id), ms: 38 + scored.length * 7, agents: ags, missing: [] };
  }

  return { diff, recording, why, decisions, ask, story };
}
