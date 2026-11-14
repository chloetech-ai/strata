// The command center's number format, big answers and column health, and
// the pure parts of its views (what a search shows, the SQL splitter, the
// permission model), in Node (tests/ui.rs runs it when node is on the
// PATH): the modules load with a little of a browser's globals.

import assert from "node:assert/strict";

const noop = () => {};
const el = () => ({ style: {}, dataset: {}, classList: { add: noop, remove: noop, toggle: noop }, append: noop, setAttribute: noop, addEventListener: noop, replaceChildren: noop, children: [] });
Object.assign(globalThis, {
  window: globalThis,
  location: { host: "x", pathname: "/", hash: "", search: "", href: "http://x/" },
  matchMedia: () => ({ matches: false, addEventListener: noop }),
  sessionStorage: { getItem: () => null, setItem: noop, removeItem: noop },
  localStorage: { getItem: () => null, setItem: noop, removeItem: noop },
  document: { addEventListener: noop, querySelector: () => null, querySelectorAll: () => [], documentElement: el(), body: el(), createElement: el, createElementNS: el, createTextNode: el },
});
const app = new URL("../ui/app/", import.meta.url);
const { fmt, ApiError } = await import(new URL("core.js", app));
const big = await import(new URL("bigresult.js", app));
const { flags } = await import(new URL("health.js", app));

// One format for a value of the data: the grid, a chart's tip, Sheets.
assert.equal(fmt.num(226159459.83), (226159459.83).toLocaleString(undefined, { maximumFractionDigits: 2 }));
assert.equal(fmt.num(4.1271186440678), (4.127119).toLocaleString(undefined, { maximumFractionDigits: 6 }));
assert.equal(fmt.num(0.00001234), "1.234e-5");
assert.equal(fmt.num(null), "–");
// Round ticks, labelled short: not 991.7M, 1.5B.
assert.deepEqual(fmt.ticks(0, 1983364195.78, 4), [0, 5e8, 1e9, 1.5e9, 2e9]);
assert.deepEqual(fmt.ticks(0, 1983364195.78, 4).map(fmt.tick), ["0", "500M", "1B", "1.5B", "2B"]);
assert.deepEqual(fmt.ticks(-3, 7.3, 4), [-5, 0, 5, 10]);

// The server's refusal of an answer for its size, in its two wordings.
assert.ok(big.tooBig(new ApiError(400, "query error: the query matches more than 10000 rows, the most a query returns; add a LIMIT (or read every row a page at a time: a scroll, page_size)")));
assert.ok(big.tooBig(new ApiError(400, "query error: LIMIT and OFFSET reach row 20000; a query returns at most 10000 rows")));
assert.ok(!big.tooBig(new ApiError(400, "unknown column x")));
assert.equal(big.SHOWN, 10000);
assert.equal(big.wholeTable("SELECT * FROM trips"), "trips");
assert.equal(big.wholeTable('select * from "my trips";'), "my trips");
assert.equal(big.wholeTable("SELECT * FROM trips WHERE fare > 3"), null);

// A column's flags: always NULL, mostly NULL, one value, a sum a few rows make.
assert.deepEqual(flags({ n: 50, nonNull: 0 }), [["bad", "always NULL"]]);
assert.deepEqual(flags({ n: 100, nonNull: 100, distinct: 1 }), [["warn", "one value"]]);
assert.deepEqual(flags({ n: 1000, nonNull: 1000, distinct: 900, sum: 100, top: 10, topSum: 80 }).map(([, t]) => t), ["10 rows make 80% of the sum"]);
assert.deepEqual(flags({ n: 1000, nonNull: 1000, distinct: 900, sum: 100, top: 10, topSum: 20 }), []);
// CCB-1: a search over a TEXT made from others (not kept, so it reads
// NULL) shows the stored fields it is made from, the title first; the
// saved search's SELECT is the same.
const tableView = await import(new URL("views/table.js", app));
const products = { table: "products", short: { key: "id", table: "products", fields: [
  { name: "id", type: "keyword" }, { name: "title", type: "text", indexed: false, positions: false }, { name: "description", type: "text", indexed: false, positions: false },
  { name: "body", type: "text", from: ["title", "description"] }, { name: "brand", type: "keyword" }, { name: "price", type: "float" },
  { name: "emb", type: "vector", embed: { from: ["title", "description"], model: "hash:64" } }] } };
const body = products.short.fields[3], emb = products.short.fields[6];
const shown = tableView.shownFields(products, [body], emb);
assert.deepEqual([shown.title, shown.text], ["title", "description"]);
assert.ok(!shown.cols.includes("body") && !shown.cols.includes("emb"), shown.cols.join());
const s = tableView.searchSql(products, { q: "rain jacket", fields: [body], vec: emb, mode: "both" });
assert.match(s.body, /^SELECT id, title, description, brand, price, score\(\)\nFROM products/);
assert.match(s.body, /bm25\(body, :q\)/);
// A stored searched field shows itself.
const docs = { table: "docs", short: { key: "id", fields: [{ name: "id", type: "keyword" }, { name: "title", type: "text" }, { name: "text", type: "text" }, { name: "n", type: "int" }] } };
assert.deepEqual(tableView.shownFields(docs, [docs.short.fields[2]]), { cols: ["id", "title", "text", "n"], title: "title", text: "text" });
// LIV-5, PRF-5: the Quality tab's profile of a wide table is a few
// statements of side-by-side aggregates and one top-3 a keyword field,
// run a few at a time; the key's distinct count is not asked.
const wideFields = [{ name: "id", type: "keyword" }, ...Array.from({ length: 30 }, (_, i) => ({ name: `k${i}`, type: "keyword" })),
  ...Array.from({ length: 30 }, (_, i) => ({ name: `n${i}`, type: "float" })), ...Array.from({ length: 30 }, (_, i) => ({ name: `t${i}`, type: "text" }))];
const wide = { table: "wide", short: { key: "id", fields: wideFields } };
const prof = tableView.profileSql(wide, wideFields);
assert.equal(prof.tops.length, 30);
assert.ok(prof.batches.length <= 14, `${prof.batches.length} statements of aggregates`);
assert.ok(prof.batches.every((b) => b.parts.length <= 26));
assert.ok(!prof.batches.some((b) => /COUNT\(DISTINCT id\)|COUNT\(id\)/.test(b.sql)));
assert.ok(!prof.batches.some((b) => /IS NOT NULL/.test(b.sql)), "a single field is counted with COUNT(field), which works unindexed");
let live = 0, peak = 0;
await tableView.bounded(Array.from({ length: 20 }, () => async () => { live++; peak = Math.max(peak, live); await new Promise((r) => setTimeout(r, 2)); live--; }), 4);
assert.equal(peak, 4);
// CCB-3: the workbench's statements: an apostrophe in a comment is not
// a string, and a ';' in a string, a quoted name or a comment ends nothing.
const sqlView = await import(new URL("views/sql.js", app));
const three = "-- the shop's first query\nSELECT 1 AS one;\nSELECT 2 AS two;\n";
assert.equal(sqlView.statements(three).length, 2);
assert.equal(sqlView.statementAt(three, [three.length - 3, three.length - 3]).sql.trim(), "SELECT 2 AS two");
assert.equal(sqlView.statementAt(three, [30, 30]).sql.trim(), "-- the shop's first query\nSELECT 1 AS one");
const tricky = `SELECT 'a;b', "x;y" FROM t /* ; it's */ WHERE s = 'it''s;'; -- done; isn't it\nSELECT 3`;
assert.deepEqual(sqlView.statements(tricky).map((x) => x.sql.trim()), [`SELECT 'a;b', "x;y" FROM t /* ; it's */ WHERE s = 'it''s;'`, "-- done; isn't it\nSELECT 3"]);
assert.equal(sqlView.statements("SELECT 1; -- only a comment\n").length, 1);
// CCB-8, CCB-9: a .json of one object a line is JSON Lines; a CSV
// separated by ';' is read as such, decimal commas as numbers.
const imp = await import(new URL("views/import.js", app));
assert.equal(await imp.sniff(new File(['{"a":1}\n{"a":2}\n'], "events.json")), "jsonl");
assert.equal(await imp.sniff(new File(['[{"a":1}]'], "rows.json")), "json");
assert.equal(await imp.sniff(new File(['{\n  "a": 1\n}'], "one.json")), "json");
assert.equal(imp.otherSeparator("id;name;amount"), ";");
assert.equal(imp.otherSeparator("id,name,amount"), null);
assert.equal(imp.otherSeparator("id|name|tags"), "|");
assert.equal(imp.toCommaCsv('id;name;amount\n1;Anna;3,5\n2;"Bo; Jr";4\n', ";"), 'id,name,amount\n1,Anna,3.5\n2,Bo; Jr,4\n');
assert.equal(imp.toCommaCsv("id;city\n1;Paris, France\n", ";"), 'id,city\n1,"Paris, France"\n');
const { isStored } = await import(new URL("catalog.js", app));
assert.deepEqual(products.short.fields.map(isStored), [true, true, true, false, true, true, false]);
assert.equal(isStored({ name: "b", type: "text", from: ["a"], stored: true }), true);

// CCW-1: the ops and propose roles. A role key reads as a read key does,
// and may take what the catalog says its role may (an ops key the bounded,
// tier 1 actions); the tooltip names the key that would do.
const core = await import(new URL("core.js", app));
const ACK = "POST /v1/alerts/:rule/ack", NOTE = "POST /v1/events/notes", APPROVE = "POST /v1/admin/oncall/proposals/:id/approve";
core.provideRoutes([
  { method: "POST", path: "/v1/alerts/:rule/ack", scope: "write", tier: 1, roles: { ops: true, propose: false } },
  { method: "POST", path: "/v1/events/notes", scope: "admin", tier: 1, roles: { ops: true, propose: false } },
  { method: "GET", path: "/v1/admin/audit", scope: "admin", tier: 0, roles: { ops: true, propose: true } },
  { method: "POST", path: "/v1/admin/oncall/proposals/:id/approve", scope: "admin", tier: 2, roles: { ops: false, propose: false } },
]);
const role = (scope) => ({ name: "bot", scope, may: { data: "read", server_views: true, act_alone_up_to_tier: scope === "ops" ? 1 : 0, reason_required: true } });
core.provide({ whoami: role("ops") });
assert.equal(core.roleOf(), "ops");
assert.ok(core.can("read") && !core.can("write") && !core.can("admin"));
assert.ok(core.can("write", ACK) && core.can("admin", NOTE) && core.can("admin", "GET /v1/admin/audit"));
assert.ok(!core.can("admin", APPROVE));
assert.equal(core.needs("admin", APPROVE), "needs an admin key (this key is ops: it files a change request instead)");
assert.ok(core.reasonNeeded());
core.provide({ whoami: role("propose") });
assert.ok(core.can("read") && !core.can("write", ACK) && core.can("admin", "GET /v1/admin/audit"));
assert.equal(core.needs("write", ACK), "needs a write or an ops key (this key is propose: it files change requests)");
core.provide({ whoami: { name: "w", scope: "write", may: { data: "write", server_views: false } } });
assert.ok(core.can("write", ACK) && !core.can("admin", NOTE) && core.can("write"));
assert.equal(core.needs("admin", NOTE), "needs an admin or an ops key");
assert.equal(core.roleOf(), null);
core.provide({ whoami: null });
core.provideRoutes(null);

// CCW-3: after a 401 the polls hold (a poll runs once, then waits) until
// another key is given.
// (Another key makes data.js poll again: its answers here are empty.)
console.warn = noop;
let refuse = true;
globalThis.fetch = async () => (refuse ? { status: 401, ok: false, text: async () => '{"error":"a key is needed"}' } : { status: 200, ok: true, text: async () => "{}" });
await core.api("/v1/alerts").catch(() => {});
assert.ok(core.signInNeeded());
const ticks = [];
const tick = () => new Promise((r) => setTimeout(r, 30));
const stop = core.every(10, () => ticks.push(1));
await tick(); await tick();
assert.equal(ticks.length, 1, "a poll waits while the key is refused");
refuse = false;
core.setToken("another");
assert.ok(!core.signInNeeded());
await tick();
assert.ok(ticks.length > 1, "it goes on once another key is given");
stop();

console.log("ok");
process.exit(0);
