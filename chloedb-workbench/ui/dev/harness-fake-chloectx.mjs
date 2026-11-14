#!/usr/bin/env node
// ./fake-chloectx.mjs with receipts as chloectx keeps them, for
// ./harness-seed.mjs: a receipt in the store may hold every candidate of
// its recall (`gate`, `inputs`, rows with their decisions). As the real
// chloectx does:
//   receipts --session S --json    lists the rows it showed only, without G
//                                  and the inputs;
//   receipt ID --json [--no-replay]  answers {receipt, records} in full
//                                  (no replays: this fake keeps no snapshot).
// Every other command is ./fake-chloectx.mjs's. Node 22.
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const STORE = process.env.WB_FAKE_CHLOECTX_STORE || path.join(here, "fake-chloectx.json");
const args = process.argv.slice(2);
const SHOWN = new Set(["take", "excerpt", "knowledge", "listed", "briefing"]);
const load = () => { try { return JSON.parse(fs.readFileSync(STORE, "utf8")); } catch { return { records: [], receipts: [] }; } };
const out = (v) => process.stdout.write(JSON.stringify(v, null, 2) + "\n");

if (args[0] === "receipt") {
  const id = args[1];
  const s = load();
  const r = (s.receipts || []).find((x) => x.id === id);
  if (!r) { process.stderr.write(`chloectx: no receipt ${id}\n`); process.exit(1); }
  const records = Object.fromEntries((r.rows || []).map((row) => [row.id, { text: row.text, tag: row.tag || "" }]));
  out({ receipt: r, records, replays: [] });
} else if (args[0] === "receipts") {
  const r = spawnSync(process.execPath, [path.join(here, "fake-chloectx.mjs"), ...args], { encoding: "utf8", env: process.env });
  if (r.status !== 0) { process.stderr.write(r.stderr || ""); process.exit(r.status || 1); }
  const v = JSON.parse(r.stdout);
  v.receipts = (v.receipts || []).map(({ gate, inputs, ...x }) => ({ ...x, rows: (x.rows || []).filter((row) => SHOWN.has(row.decision)) }));
  out(v);
} else {
  const r = spawnSync(process.execPath, [path.join(here, "fake-chloectx.mjs"), ...args], { stdio: "inherit", env: process.env });
  process.exit(r.status ?? 1);
}
