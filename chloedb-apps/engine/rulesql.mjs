// CREATE RULE: rules written as SQL. A rule (engine/rules.mjs) is a query, or
// steps the engine runs as one transaction; written as JSON it is long and
// unlike anything a model or a person has seen. Written as SQL it reads like
// a stored procedure:
//
//   CREATE RULE checkout AS
//     REQUIRE (SELECT COUNT(*) FROM cart WHERE owner = :user.id) > 0 ELSE 'Your cart is empty';
//     INSERT INTO orders (customer, status) VALUES (:user.id, 'new') AS order;
//     FOR line IN (SELECT c.id, c.product, c.qty, p.price, p.stock, p.name FROM cart c JOIN products p ON p.id = c.product WHERE c.owner = :user.id) DO
//       REQUIRE :line.stock >= :line.qty ELSE 'Only :line.stock left of :line.name';
//       INSERT INTO order_lines (order_id, product, qty, price) VALUES (:order.id, :line.product, :line.qty, :line.price);
//       UPDATE products SET stock = stock - :line.qty WHERE id = :line.product;
//       DELETE FROM cart WHERE id = :line.id;
//     END;
//     RETURN :order.id;
//
//   CREATE RULE count_votes AFTER INSERT OR DELETE ON votes AS
//     UPDATE features SET votes = (SELECT COUNT(*) FROM votes WHERE feature = :row.feature) WHERE id = :row.feature;
//
//   CREATE RULE spent RETURNS VALUE AS SELECT SUM(amount) FROM expenses WHERE day >= :from;
//
// compileRule() turns a statement into those steps, so the engine, its checks
// and its transaction are the same; the statement is kept as the rule's
// source, shown to the model and to people instead of the JSON.

import { parseEvery, everyText, ScheduleError } from './schedule.mjs';
import { FEED_FIELDS } from './feeds.mjs';

export class RuleSqlError extends Error {}
const fail = (msg) => { throw new RuleSqlError(msg); };

const SELECT_RE = /^\s*(select|with)\b/i;
const NAME = '[A-Za-z_][A-Za-z0-9_]*';
const PARAM_RE = new RegExp(`^:${NAME}(?:\\.${NAME})?$`);
const EVENTS = ['insert', 'update', 'delete'];

// Drops -- comments, outside quotes.
function uncomment(text) {
  let out = '';
  for (let i = 0, q = null; i < text.length; i++) {
    const ch = text[i];
    if (q) { out += ch; if (ch === q) q = null; continue; }
    if (ch === "'" || ch === '"') { q = ch; out += ch; continue; }
    if (ch === '-' && text[i + 1] === '-') { while (i < text.length && text[i] !== '\n') i++; out += '\n'; continue; }
    out += ch;
  }
  return out;
}

// Walks text at the top level: fn(i, depth) for each character outside
// quotes; stops when fn returns true. Returns the index it stopped at, or -1.
function scan(text, fn) {
  let depth = 0;
  for (let i = 0, q = null; i < text.length; i++) {
    const ch = text[i];
    if (q) { if (ch === q) q = null; continue; }
    if (ch === "'" || ch === '"') { q = ch; continue; }
    if (ch === '(') depth++;
    else if (ch === ')') depth--;
    else if (fn(i, depth)) return i;
  }
  return -1;
}

// Splits at a character found at the top level (";" or ",").
function splitTop(text, sep) {
  const parts = [];
  let from = 0;
  scan(text, (i, d) => { if (d === 0 && text[i] === sep) { parts.push(text.slice(from, i)); from = i + 1; } return false; });
  parts.push(text.slice(from));
  return parts;
}

// The index of the first top-level keyword (a word, matched whole), or -1.
function findWord(text, word, from = 0) {
  const re = new RegExp(`^${word}\\b`, 'i');
  return scan(text, (i, d) => d === 0 && i >= from && (i === 0 || /[\s)]/.test(text[i - 1])) && re.test(text.slice(i, i + word.length + 1)));
}

// "(...)" at the start of text: [inside, rest], or null.
function group(text) {
  const t = text.trimStart();
  if (!t.startsWith('(')) return null;
  let depth = 0;
  for (let i = 0, q = null; i < t.length; i++) {
    const ch = t[i];
    if (q) { if (ch === q) q = null; continue; }
    if (ch === "'" || ch === '"') { q = ch; continue; }
    if (ch === '(') depth++;
    else if (ch === ')' && --depth === 0) return [t.slice(1, i), t.slice(i + 1)];
  }
  return fail(`a "(" has no ")": ${t.slice(0, 40)}`);
}

const unquote = (s) => s.slice(1, -1).replace(/''/g, "'");
const isWholeGroup = (e) => { const g = e.startsWith('(') && group(e); return !!g && !g[1].trim(); };

// A value as a rule step holds it: ":name", a literal, or a SELECT whose first
// value it is.
export function valueOf(expr) {
  const e = String(expr).trim();
  if (!e) fail('a value is missing');
  if (PARAM_RE.test(e)) return e;
  if (/^'(?:[^']|'')*'$/.test(e)) { const s = unquote(e); return SELECT_RE.test(s) || s.startsWith(':') ? `SELECT ${e}` : s; }
  if (/^-?\d+(\.\d+)?$/.test(e)) return Number(e);
  if (/^null$/i.test(e)) return null;
  if (/^(true|false)$/i.test(e)) return /^true$/i.test(e);
  if (isWholeGroup(e)) { const inner = group(e)[0].trim(); return SELECT_RE.test(inner) ? inner : valueOf(inner); }
  if (SELECT_RE.test(e)) return e;
  return `SELECT ${e}`;
}

// A value as SQL text, to put inside another query.
const sqlOf = (expr) => { const e = String(expr).trim(); return PARAM_RE.test(e) || /^'(?:[^']|'')*'$/.test(e) || /^-?\d+(\.\d+)?$/.test(e) || /^(null|true|false)$/i.test(e) || isWholeGroup(e) ? e : `(${e})`; };
// A condition as a SELECT whose first value decides.
const condOf = (expr) => { const e = String(expr).trim(); if (SELECT_RE.test(e)) return e; if (isWholeGroup(e)) { const inner = group(e)[0].trim(); if (SELECT_RE.test(inner)) return inner; } return `SELECT ${e}`; };
// A step that runs only when a condition (a SELECT) holds, and its own.
const only = (s, cond) => ({ ...s, if: s.if ? `SELECT (${cond}) AND (${s.if})` : cond });

let seq = 0;
const tmp = (p) => `_${p}${++seq}`;

// UPDATE t SET a = e, ... : each value; an expression over the row's own
// columns (stock - :qty) is read from the row.
function setOf(assigns, table, idSql) {
  const set = {};
  for (const a of splitTop(assigns, ',')) {
    const m = a.match(new RegExp(`^\\s*(${NAME})\\s*=\\s*([\\s\\S]+)$`));
    if (!m) fail(`SET ${a.trim().slice(0, 40)}: write column = value`);
    const e = m[2].trim();
    const v = valueOf(e);
    set[m[1]] = typeof v === 'string' && v === `SELECT ${e}` ? `SELECT ${e} FROM ${table} WHERE id = ${idSql}` : v;
  }
  return set;
}

// The statements of a rule's body, as steps.
function parseList(q, closers) {
  const steps = [];
  while (q.length) {
    const c = q[0].trim();
    if (!c) { q.shift(); continue; }
    const end = c.match(/^END(?:\s+(?:FOR|IF|LOOP))?\b/i);
    if (end) {
      if (!closers.includes('END')) fail('END without FOR or IF');
      q[0] = c.slice(end[0].length);
      return { steps, closer: 'END' };
    }
    if (/^ELSE\b/i.test(c) && closers.includes('ELSE')) { q[0] = c.slice(4); return { steps, closer: 'ELSE' }; }
    q.shift();
    steps.push(...parseStatement(c, q));
  }
  if (closers.length) fail('a FOR or IF has no END');
  return { steps, closer: null };
}

function parseStatement(c, q) {
  let m;
  // FOR name IN (SELECT ...) DO ... END
  if ((m = c.match(new RegExp(`^FOR\\s+(?:EACH\\s+)?(${NAME})\\s+IN\\b`, 'i')))) {
    let rest = c.slice(m[0].length).trim();
    let sql;
    if (rest.startsWith('(')) { const g = group(rest); sql = g[0].trim(); rest = g[1].trim(); }
    else { const at = findWord(rest, 'DO'); if (at < 0) fail(`FOR ${m[1]}: write FOR ${m[1]} IN (SELECT ...) DO ... END`); sql = rest.slice(0, at).trim(); rest = rest.slice(at); }
    if (!/^DO\b/i.test(rest)) fail(`FOR ${m[1]} IN (...): DO is missing`);
    if (!SELECT_RE.test(sql)) fail(`FOR ${m[1]} IN: give a SELECT; its rows are gone through one by one`);
    q.unshift(rest.slice(2));
    const body = parseList(q, ['END']);
    if (!body.steps.length) fail(`FOR ${m[1]}: no statements between DO and END`);
    return [{ each: sql, as: m[1], do: body.steps }];
  }
  // IF cond THEN ... [ELSE ...] END
  if (/^IF\b/i.test(c)) {
    const at = findWord(c, 'THEN');
    if (at < 0) fail('IF: write IF condition THEN ... END');
    const cond = c.slice(2, at).trim();
    q.unshift(c.slice(at + 4));
    const yes = parseList(q, ['END', 'ELSE']);
    const no = yes.closer === 'ELSE' ? parseList(q, ['END']).steps : [];
    const when = condOf(cond);
    const unless = `SELECT NOT ((${when}))`;
    return [...yes.steps.map((s) => only(s, when)), ...no.map((s) => only(s, unless))];
  }
  // REQUIRE cond [ELSE 'message']
  if (/^REQUIRE\b/i.test(c)) {
    const body = c.slice(7);
    const at = findWord(body, 'ELSE');
    const cond = (at < 0 ? body : body.slice(0, at)).trim();
    if (!cond) fail('REQUIRE: give the condition that must hold');
    const step = { require: condOf(cond) };
    if (at >= 0) {
      const msg = body.slice(at + 4).trim();
      if (!/^'(?:[^']|'')*'$/.test(msg)) fail("REQUIRE ... ELSE: the message is text in single quotes ('Out of stock')");
      step.else = unquote(msg);
    }
    return [step];
  }
  // LET name = value  (or = CALL fn(...))
  if ((m = c.match(new RegExp(`^LET\\s+(${NAME})\\s*(?::=|=)\\s*([\\s\\S]+)$`, 'i')))) {
    if (/^CALL\b/i.test(m[2].trim())) return parseStatement(`${m[2].trim()} AS ${m[1]}`, q);
    return [{ let: m[1], value: valueOf(m[2]) }];
  }
  if ((m = c.match(/^RETURN\s+([\s\S]+)$/i))) return [{ return: valueOf(m[1]) }];
  // SEND EMAIL TO value SUBJECT 'text' [TEXT 'text'] [ROWS (SELECT ...)] [NONE 'text']
  if (/^SEND\s+EMAIL\b/i.test(c)) {
    const body = c.replace(/^SEND\s+EMAIL\s*/i, '');
    const how = "SEND EMAIL TO :to SUBJECT 'New this week' TEXT 'Hello :user.name' ROWS (SELECT title, created FROM requests WHERE ...)";
    const at = ['TO', 'SUBJECT', 'TEXT', 'ROWS', 'NONE'].map((w) => [w, findWord(body, w)]).filter(([, i]) => i >= 0).sort((a, b) => a[1] - b[1]);
    if (!at.length || body.slice(0, at[0][1]).trim()) fail(`SEND EMAIL: write ${how}`);
    const part = {};
    at.forEach(([w, i], k) => { part[w] = body.slice(i + w.length, k + 1 < at.length ? at[k + 1][1] : body.length).trim(); });
    if (!part.TO) fail(`SEND EMAIL: say who it goes to (TO :to, TO :user.email, TO (SELECT email FROM ...)): ${how}`);
    const text = (w) => {
      if (part[w] === undefined) return undefined;
      if (!/^'(?:[^']|'')*'$/.test(part[w])) fail(`SEND EMAIL ... ${w}: the text in single quotes (:name in it is filled in)`);
      return unquote(part[w]);
    };
    const email = { to: valueOf(part.TO) };
    if (part.SUBJECT !== undefined) email.subject = text('SUBJECT');
    if (part.TEXT !== undefined) email.text = text('TEXT');
    if (!email.subject && !email.text) fail(`SEND EMAIL: give it a SUBJECT or a TEXT: ${how}`);
    if (part.ROWS !== undefined) {
      const g = part.ROWS.startsWith('(') ? group(part.ROWS) : null;
      const sql = (g && !g[1].trim() ? g[0] : part.ROWS).trim();
      if (!SELECT_RE.test(sql)) fail('SEND EMAIL ... ROWS: give (SELECT ...), one line of the email per row');
      email.rows = sql;
    }
    if (part.NONE !== undefined) email.none = text('NONE');
    return [{ email }];
  }
  // CALL fn(name => value, ...) [AS name]
  if ((m = c.match(/^CALL\s+([a-z][a-z0-9_-]*)\s*/i))) {
    let rest = c.slice(m[0].length);
    const input = {};
    if (rest.trim().startsWith('(')) {
      const g = group(rest);
      rest = g[1];
      for (const a of splitTop(g[0], ',').filter((x) => x.trim())) {
        const am = a.match(new RegExp(`^\\s*(${NAME})\\s*(?:=>|:=|=)\\s*([\\s\\S]+)$`));
        if (!am) fail(`CALL ${m[1]}: name each input (CALL ${m[1]}(item => :row.id))`);
        input[am[1]] = valueOf(am[2]);
      }
    }
    const as = rest.match(new RegExp(`^\\s*AS\\s+(${NAME})\\s*$`, 'i'));
    if (rest.trim() && !as) fail(`CALL ${m[1]}: unexpected "${rest.trim().slice(0, 30)}"`);
    return [{ call: m[1], input, ...(as ? { as: as[1] } : {}) }];
  }
  // UPSERT INTO t (cols) VALUES (values) FIND (SELECT id ...) [AS name]: the
  // row FIND selects gets the values, or a new row does.
  if ((m = c.match(new RegExp(`^UPSERT\\s+INTO\\s+(${NAME})\\s*`, 'i')))) {
    const table = m[1];
    const cols = group(c.slice(m[0].length));
    if (!cols) fail(`UPSERT INTO ${table}: list the columns`);
    let rest = cols[1].trim();
    if (!/^VALUES\b/i.test(rest)) fail(`UPSERT INTO ${table}: give VALUES (...)`);
    const vals = group(rest.slice(6));
    rest = vals[1].trim();
    const names = splitTop(cols[0], ',').map((x) => x.trim()).filter(Boolean);
    const exprs = splitTop(vals[0], ',').map((x) => x.trim()).filter(Boolean);
    if (names.length !== exprs.length) fail(`UPSERT INTO ${table}: ${names.length} columns but ${exprs.length} values`);
    if (!/^FIND\b/i.test(rest)) fail(`UPSERT INTO ${table}: give FIND (SELECT id FROM ${table} WHERE ...), the row to update`);
    const find = group(rest.slice(4));
    if (!find) fail(`UPSERT INTO ${table}: FIND (SELECT ...)`);
    const as = find[1].match(new RegExp(`^\\s*AS\\s+(${NAME})\\s*$`, 'i'));
    if (find[1].trim() && !as) fail(`UPSERT INTO ${table}: unexpected "${find[1].trim().slice(0, 30)}"`);
    return [{ upsert: table, find: find[0].trim(), values: Object.fromEntries(names.map((n, i) => [n, valueOf(exprs[i])])), ...(as ? { as: as[1] } : {}) }];
  }
  // INSERT INTO t (cols) VALUES (values) [ON CONFLICT (cols) DO NOTHING | DO UPDATE SET ...] [AS name]
  if ((m = c.match(new RegExp(`^INSERT\\s+INTO\\s+(${NAME})\\s*`, 'i')))) {
    const table = m[1];
    const cols = group(c.slice(m[0].length));
    if (!cols) fail(`INSERT INTO ${table}: list the columns (INSERT INTO ${table} (a, b) VALUES (...))`);
    let rest = cols[1].trim();
    if (!/^VALUES\b/i.test(rest)) fail(`INSERT INTO ${table}: give VALUES (...); to add rows from a query, use FOR row IN (SELECT ...) DO INSERT ... END`);
    const vals = group(rest.slice(6));
    if (!vals) fail(`INSERT INTO ${table}: VALUES (...) is missing its values`);
    rest = vals[1].trim();
    const names = splitTop(cols[0], ',').map((x) => x.trim());
    const exprs = splitTop(vals[0], ',').map((x) => x.trim());
    if (names.length !== exprs.length) fail(`INSERT INTO ${table}: ${names.length} columns but ${exprs.length} values`);
    for (const n of names) if (!new RegExp(`^${NAME}$`).test(n)) fail(`INSERT INTO ${table}: ${n} is not a column name`);
    const values = Object.fromEntries(names.map((n, i) => [n, valueOf(exprs[i])]));
    const asAt = rest.match(new RegExp(`\\bAS\\s+(${NAME})\\s*$`, 'i'));
    const as = asAt ? asAt[1] : null;
    if (asAt) rest = rest.slice(0, asAt.index).trim();
    const named = as ? { as } : {};
    if (!rest) return [{ insert: table, values, ...named }];
    const oc = rest.match(/^ON\s+CONFLICT\s*/i);
    if (!oc) fail(`INSERT INTO ${table}: unexpected "${rest.slice(0, 30)}"`);
    const keys = group(rest.slice(oc[0].length));
    if (!keys) fail(`INSERT INTO ${table} ... ON CONFLICT: name the columns that find the row (ON CONFLICT (owner, product))`);
    const keyCols = splitTop(keys[0], ',').map((x) => x.trim());
    for (const k of keyCols) if (!names.includes(k)) fail(`ON CONFLICT (${k}): ${k} is not among the inserted columns`);
    const exprOf = Object.fromEntries(names.map((n, i) => [n, exprs[i]]));
    const find = `SELECT id FROM ${table} WHERE ${keyCols.map((k) => `${k} = ${sqlOf(exprOf[k])}`).join(' AND ')}`;
    const then = keys[1].trim();
    const found = tmp('found');
    if (/^DO\s+NOTHING$/i.test(then)) return [{ let: found, value: find }, { insert: table, values, if: `SELECT :${found} IS NULL`, ...named }];
    const up = then.match(/^DO\s+UPDATE\s+SET\s+([\s\S]+)$/i);
    if (!up) fail(`INSERT INTO ${table} ... ON CONFLICT (...): then DO NOTHING or DO UPDATE SET col = value`);
    const assigns = up[1].replace(new RegExp(`\\bexcluded\\.(${NAME})\\b`, 'gi'), (x, col) => { if (!(col in exprOf)) fail(`excluded.${col}: ${col} is not among the inserted columns`); return sqlOf(exprOf[col]); });
    return [{ let: found, value: find }, { insert: table, values, if: `SELECT :${found} IS NULL`, ...named }, { update: table, id: `:${found}`, set: setOf(assigns, table, `:${found}`), ...named }];
  }
  // UPDATE t SET ... WHERE id = value | WHERE condition
  if ((m = c.match(new RegExp(`^UPDATE\\s+(${NAME})\\s+SET\\s+`, 'i')))) {
    const table = m[1];
    let rest = c.slice(m[0].length);
    let asAt = rest.match(new RegExp(`\\s+AS\\s+(${NAME})\\s*$`, 'i'));
    if (asAt && findWord(rest, 'AS', asAt.index) !== asAt.index + asAt[0].search(/AS/i)) asAt = null;
    if (asAt) rest = rest.slice(0, asAt.index);
    const w = findWord(rest, 'WHERE');
    if (w < 0) fail(`UPDATE ${table}: give WHERE (WHERE id = :row.feature, or a condition)`);
    return writeWhere(table, rest.slice(w + 5).trim(), (idSql, id) => ({ update: table, id, set: setOf(rest.slice(0, w), table, idSql), ...(asAt ? { as: asAt[1] } : {}) }));
  }
  if ((m = c.match(new RegExp(`^DELETE\\s+FROM\\s+(${NAME})\\s*`, 'i')))) {
    const table = m[1];
    const rest = c.slice(m[0].length);
    if (!/^WHERE\b/i.test(rest)) fail(`DELETE FROM ${table}: give WHERE (WHERE id = :row.hold, or a condition)`);
    return writeWhere(table, rest.slice(5).trim(), (idSql, id) => ({ remove: table, id }));
  }
  // PULL FEED 'url' INTO t (cols) KEY col: a feed's (or a JSON list's) new items as rows
  if ((m = c.match(/^PULL\s+(FEED|RSS|ATOM|JSON)\s+/i))) return [pullOf(c.slice(m[0].length), m[1], 'PULL')];
  if (SELECT_RE.test(c)) fail('a SELECT among other statements does nothing: name its value (LET total = (SELECT ...)), check it (REQUIRE ...) or give it back (RETURN (SELECT ...)); a rule that is only a query is CREATE RULE name AS SELECT ...');
  return fail(`"${c.slice(0, 40)}": a rule's statements are REQUIRE, LET, INSERT, UPDATE, DELETE, CALL, SEND EMAIL, FOR ... DO ... END, IF ... THEN ... END, PULL and RETURN`);
}

// PULL FEED | JSON 'url' (or :name) [AT 'path'] [SECRET 'name'] INTO t (col [= field], ...) KEY col [LIMIT n] [AS name]
function pullOf(src, kind, what) {
  let rest = String(src).trim();
  const from = /^json$/i.test(kind) ? 'json' : 'feed';
  let m = rest.match(/^'(?:[^']|'')*'/) || rest.match(new RegExp(`^:${NAME}(?:\\.${NAME})?`));
  if (!m) fail(`${what} ${from.toUpperCase()}: give the address in single quotes ('https://example.com/feed.xml') or as :name`);
  const url = m[0].startsWith("'") ? unquote(m[0]) : m[0];
  if (url.startsWith("'") || (!url.startsWith(':') && !/^https?:\/\//i.test(url))) fail(`${what} ${from.toUpperCase()}: ${url.slice(0, 60)} is not an http or https address`);
  rest = rest.slice(m[0].length).trim();
  const out = { from, url };
  const word = (re) => { const x = rest.match(re); if (x) rest = rest.slice(x[0].length).trim(); return x; };
  let x;
  if ((x = word(/^AT\s+('(?:[^']|'')*')/i))) {
    if (from !== 'json') fail(`${what} FEED: AT says where the list is in a JSON answer; a feed's items are found by themselves`);
    out.at = unquote(x[1]);
  }
  if ((x = word(/^SECRET\s+'([A-Za-z0-9_-]{1,40})'/i))) out.secret = x[1];
  if (!(x = word(new RegExp(`^INTO\\s+(${NAME})\\s*`, 'i')))) fail(`${what}: say which table gets the items: INTO stories (title, link, published, summary) KEY link`);
  out.into = x[1];
  const g = group(rest);
  if (!g) fail(`${what} ... INTO ${out.into}: list the columns it fills in parentheses, each a field of the item (title, link) or column = field (headline = title)`);
  rest = g[1].trim();
  out.cols = {};
  for (const part of splitTop(g[0], ',').map((p) => p.trim()).filter(Boolean)) {
    const pm = part.match(new RegExp(`^(${NAME})(?:\\s*=\\s*([A-Za-z_][A-Za-z0-9_.:-]*))?$`));
    if (!pm) fail(`${what} ... INTO ${out.into} (${part.slice(0, 30)}): write a column, or column = field`);
    out.cols[pm[1]] = pm[2] || pm[1];
  }
  if (!Object.keys(out.cols).length) fail(`${what} ... INTO ${out.into}: no columns`);
  if (from === 'feed') for (const [col, f] of Object.entries(out.cols)) if (!FEED_FIELDS.includes(f)) fail(`${what} FEED ... ${col}${f !== col ? ` = ${f}` : ''}: a feed item's fields are ${FEED_FIELDS.join(', ')}`);
  if (!(x = word(new RegExp(`^KEY\\s+(${NAME})`, 'i')))) fail(`${what} ... INTO ${out.into} (...): give KEY col, the column that tells items apart (such as link), so an item is added once`);
  out.key = x[1];
  if (!(out.key in out.cols)) fail(`${what} ... KEY ${out.key}: ${out.key} is not among the columns it fills`);
  if ((x = word(/^LIMIT\s+(\d+)/i))) out.limit = Math.max(1, Math.min(200, Number(x[1])));
  const as = word(new RegExp(`^AS\\s+(${NAME})$`, 'i'));
  if (rest) fail(`${what}: "${rest.slice(0, 30)}" is not understood; the clauses are AT, SECRET, INTO, KEY, LIMIT and AS, in that order`);
  return { pull: out, ...(as ? { as: as[1] } : {}) };
}


// WHERE id = value writes one row; any other condition, each row it selects.
function writeWhere(table, cond, step) {
  const one = cond.match(/^id\s*=\s*([\s\S]+)$/i);
  if (one && findWord(one[1], 'AND') < 0 && findWord(one[1], 'OR') < 0) {
    const v = valueOf(one[1]);
    return [step(sqlOf(one[1]), typeof v === 'number' ? String(v) : v)];
  }
  const r = tmp('row');
  return [{ each: `SELECT id FROM ${table} WHERE ${cond}`, as: r, do: [step(`:${r}.id`, `:${r}.id`)] }];
}

// The statements of a body (also a test's "do", given as text), as steps.
export function compileSteps(text) {
  const q = splitTop(uncomment(String(text || '')), ';');
  return parseList(q, []).steps;
}

// CREATE RULE name [(inputs)] [RETURNS ROWS | ROW | VALUE]
//   [AFTER INSERT [OR UPDATE] [OR DELETE] ON table [RUNS AS APP]] AS body
// RUNS AS APP: a trigger that reaches every row, not only what the person
// whose write fired it may (access.mjs).
// -> { name, on, rule }
export function compileRule(text) {
  seq = 0;
  const src = uncomment(String(text || '')).trim();
  if (/^CREATE\s+(?:OR\s+REPLACE\s+)?SOURCE\b/i.test(src)) return compileSource(src);
  const m = src.match(new RegExp(`^CREATE\\s+(?:OR\\s+REPLACE\\s+)?RULE\\s+([A-Za-z][A-Za-z0-9_-]*)\\s*`, 'i'));
  if (!m) fail('start with CREATE RULE name');
  let rest = src.slice(m[0].length);
  if (rest.startsWith('(')) rest = group(rest)[1].trim(); // the inputs, named for people: :name in the body
  // RUNS AS APP | PERSON, before the AS of the body.
  let runs = null;
  const first = findWord(rest, 'AS');
  if (first > 0 && /\bRUNS\s*$/i.test(rest.slice(0, first))) {
    const who = rest.slice(first + 2).match(/^\s+(APP|PERSON|CALLER)\b/i);
    if (!who) fail(`CREATE RULE ${m[1]}: RUNS AS takes APP (every row) or PERSON (what the person whose write fired it may reach)`);
    runs = who[1].toUpperCase() === 'APP' ? 'app' : null;
    rest = rest.slice(0, first).replace(/\bRUNS\s*$/i, ' ') + rest.slice(first + 2 + who[0].length);
  }
  const as = findWord(rest, 'AS');
  if (as < 0) fail(`CREATE RULE ${m[1]}: AS is missing before the body`);
  const head = rest.slice(0, as).trim();
  const body = rest.slice(as + 2).trim().replace(/;\s*$/, '');
  let on = null;
  let returns = 'rows';
  let h = head;
  const ret = h.match(/\bRETURNS\s+(ROWS|ROW|VALUE)\b/i);
  if (ret) { returns = ret[1].toLowerCase(); h = h.replace(ret[0], ' '); }
  let every = null;
  const ev = findWord(h, 'EVERY');
  if (ev >= 0) {
    const tail = h.slice(ev);
    const stop = [findWord(tail, 'AFTER', 1), findWord(tail, 'RETURNS', 1)].filter((i) => i > 0);
    const end = stop.length ? Math.min(...stop) : tail.length;
    every = everyOf(m[1], tail.slice(0, end));
    h = h.slice(0, ev) + ' ' + tail.slice(end);
  }
  const after = h.match(new RegExp(`\\bAFTER\\s+((?:INSERT|UPDATE|DELETE)(?:\\s*(?:OR|,)\\s*(?:INSERT|UPDATE|DELETE))*)\\s+ON\\s+(${NAME})\\b`, 'i'));
  if (after) {
    const events = [...new Set(after[1].toLowerCase().split(/\s*(?:or|,)\s*/))];
    on = { table: after[2], event: events.length === 1 ? events[0] : events };
    h = h.replace(after[0], ' ');
  }
  if (h.trim()) fail(`CREATE RULE ${m[1]}: "${h.trim().slice(0, 40)}" is not understood; before AS come RETURNS ROW | VALUE, AFTER INSERT [OR UPDATE] [OR DELETE] ON table, and EVERY ...`);
  if (every && on) fail(`CREATE RULE ${m[1]}: a rule runs on a schedule (EVERY) or after writes (AFTER ... ON), not both`);
  if (runs && !on) fail(`CREATE RULE ${m[1]}: RUNS AS APP is for a trigger (AFTER ... ON); a rule people call runs as them, and a schedule runs as the app`);
  if (!body) fail(`CREATE RULE ${m[1]}: the body after AS is empty`);
  const parts = splitTop(body, ';').filter((x) => x.trim());
  if (parts.length === 1 && SELECT_RE.test(parts[0])) {
    if (on) fail(`CREATE RULE ${m[1]}: a trigger writes; a SELECT alone changes nothing`);
    if (every) fail(`CREATE RULE ${m[1]}: a scheduled rule writes, pulls or sends; a SELECT alone on a schedule changes nothing`);
    return { name: m[1], on: null, every: null, rule: { query: parts[0].trim(), ...(returns === 'row' ? { one: true } : returns === 'value' ? { value: true } : {}) } };
  }
  if (ret) fail(`CREATE RULE ${m[1]}: RETURNS is for a rule that is one SELECT; steps give back a value with RETURN`);
  const steps = compileSteps(body);
  if (!steps.length) fail(`CREATE RULE ${m[1]}: no statements`);
  return { name: m[1], on, every, ...(runs ? { as: runs } : {}), rule: { steps } };
}

function everyOf(name, text) {
  try { return parseEvery(text); } catch (e) {
    if (e instanceof ScheduleError) fail(`${name}: ${e.message}`);
    throw e;
  }
}

// CREATE SOURCE name FROM FEED | JSON 'url' [AT 'path'] [SECRET 'name'] EVERY ...
//   INTO table (col [= field], ...) KEY col [LIMIT n]
// -> a rule that is one PULL step, on a schedule.
function compileSource(src) {
  const m = src.match(/^CREATE\s+(?:OR\s+REPLACE\s+)?SOURCE\s+([A-Za-z][A-Za-z0-9_-]*)\s+FROM\s+(FEED|RSS|ATOM|JSON)\s+/i);
  if (!m) fail("write CREATE SOURCE name FROM FEED 'https://...' EVERY 1 HOUR INTO table (title, link, published, summary) KEY link");
  let rest = src.slice(m[0].length).replace(/;\s*$/, '');
  const ev = findWord(rest, 'EVERY');
  const into = findWord(rest, 'INTO');
  if (ev < 0 || into < 0 || into < ev) fail(`CREATE SOURCE ${m[1]}: give EVERY ... (how often it is read) before INTO table (...)`);
  const every = everyOf(`CREATE SOURCE ${m[1]}`, rest.slice(ev, into));
  rest = `${rest.slice(0, ev)} ${rest.slice(into)}`;
  const step = pullOf(rest, m[2], `CREATE SOURCE ${m[1]}`);
  if (step.as) fail(`CREATE SOURCE ${m[1]}: AS is for a PULL inside a rule`);
  if (step.pull.url.startsWith(':')) fail(`CREATE SOURCE ${m[1]}: give the address in single quotes; for addresses kept in a table, write a rule that does FOR f IN (SELECT url FROM feeds) DO PULL FEED :f.url ... END`);
  return { name: m[1], on: null, every, source: true, rule: { steps: [step] } };
}

export const isRuleStatement = (code) => typeof code === 'string' && /^\s*(--[^\n]*\n\s*)*CREATE\s+(OR\s+REPLACE\s+)?(RULE|SOURCE)\b/i.test(code);

// A plan's CREATE RULE statements (in its functions' code blocks) become
// rules, with their triggers; the statement is kept as the rule's source.
export function compileRuleStatements(plan, code) {
  const problems = [];
  for (const f of plan?.functions || []) {
    if (!f || !isRuleStatement(code[f.slug])) continue;
    try {
      const out = compileRule(code[f.slug]);
      f.rule = out.rule;
      f.on = out.on;
      f.every = out.every || null;
      if (out.as) f.as = out.as; else delete f.as;
      if (out.source) f.source = true; else delete f.source;
      f.statement = code[f.slug].trim();
    } catch (e) {
      if (!(e instanceof RuleSqlError)) throw e;
      problems.push(`function ${f.slug}: ${/^\s*(--[^\n]*\n\s*)*CREATE\s+(OR\s+REPLACE\s+)?SOURCE\b/i.test(code[f.slug]) ? 'CREATE SOURCE' : 'CREATE RULE'}: ${e.message}`);
      delete f.rule;
    }
  }
  // A test's "do" written as statements becomes steps.
  for (const [i, t] of (Array.isArray(plan?.tests) ? plan.tests : []).entries()) {
    if (!t || typeof t.do !== 'string') continue;
    try { t.do = compileSteps(t.do); } catch (e) {
      if (!(e instanceof RuleSqlError)) throw e;
      problems.push(`tests[${i}]${t.name ? ` (${t.name})` : ''}.do: ${e.message}`);
      t.do = [];
    }
  }
  return problems;
}

export const RULE_SQL_DOC = `# Rules: functions and triggers written as SQL

A function that is a query, or does a few writes (a checkout, an invoice from hours, a purchase order, a stock move), or a trigger that keeps a count, a total or a stock level right, is a rule: write it as one CREATE RULE statement in its function's code block, with no "rule" in its plan entry. The function keeps its slug, lowercase with dashes, in the plan and on the code block (<code slug="count-votes">CREATE RULE count_votes ...</code>, called as app.call('count-votes')); the name after CREATE RULE is only for reading. The engine runs it directly. Write JavaScript only for what a rule cannot say (text or date logic beyond SQL and its functions, the model, outside services): email is a SEND EMAIL statement, and a list's CSV is its EXPORT clause on the screen.

A query: CREATE RULE spent [RETURNS ROW | VALUE] AS SELECT SUM(amount) AS total FROM expenses WHERE day >= :from
  app.call('spent', {from: '2026-11-01'}) gives the rows (ROW: the first row; VALUE: its first column). Similar rows and "bought together" are queries too (similarity, ppr).

Statements, each ending with ";", run in order:
CREATE RULE checkout AS
  REQUIRE (SELECT COUNT(*) FROM cart WHERE owner = :user.id) > 0 ELSE 'Your cart is empty';
  LET total = (SELECT SUM(c.qty * p.price) FROM cart c JOIN products p ON p.id = c.product WHERE c.owner = :user.id);
  INSERT INTO orders (customer, status, total) VALUES (:user.id, 'new', :total) AS order;
  FOR line IN (SELECT c.id, c.product, c.qty, p.price, p.stock, p.name FROM cart c JOIN products p ON p.id = c.product WHERE c.owner = :user.id) DO
    REQUIRE :line.stock >= :line.qty ELSE 'Only :line.stock left of :line.name';
    INSERT INTO order_lines (order_id, product, qty, price) VALUES (:order.id, :line.product, :line.qty, :line.price);
    UPDATE products SET stock = stock - :line.qty WHERE id = :line.product;
    DELETE FROM cart WHERE id = :line.id;
  END;
  IF :total > 100 THEN
    UPDATE orders SET status = 'review' WHERE id = :order.id;
  END;
  RETURN :order.id;

- REQUIRE condition [ELSE 'message']: when false, the rule stops, writes nothing, and the person sees the message (:name in it is filled in).
- LET name = value: a name later statements use as :name. LET name = CALL fn(...) keeps what another function gives back.
- INSERT INTO t (cols) VALUES (values) [AS name]: AS names the new row (:order.id). Add or update in one: INSERT INTO cart (owner, product, qty) VALUES (:user.id, :product, 1) ON CONFLICT (owner, product) DO UPDATE SET qty = qty + excluded.qty (or DO NOTHING).
- UPDATE t SET col = value, ... WHERE id = value (one row), or WHERE any condition (each row it selects). A value may use the row's own columns (stock - :line.qty).
- DELETE FROM t WHERE id = value, or WHERE any condition.
- CALL fn(name => value, ...) [AS name]: another function of the app.
- FOR name IN (SELECT ...) DO statements END: once per row, the row as :name.col (at most 1000 rows).
- IF condition THEN statements [ELSE statements] END.
- RETURN value: what app.call gives back.
- SEND EMAIL TO value SUBJECT 'text' [TEXT 'text'] [ROWS (SELECT ...)] [NONE 'text']: :name in the texts is filled in; ROWS adds a line per row (its values joined by " · "), NONE is what stands there when it has none. TO is :to, :user.email, 'a@b.c' or (SELECT email FROM ...) for several (at most 10). It goes out after the rule's writes are stored, never when the rule stops. A weekly digest: CREATE RULE digest AS SEND EMAIL TO :to SUBJECT 'New requests this week' ROWS (SELECT title, votes FROM requests WHERE days_between(created, :today) <= 7 ORDER BY votes DESC) NONE 'Nothing new this week.';
- A condition or value is SQL: :name (the call's input, a LET, an AS), :row.col, a literal, an expression (:qty * :price), or a (SELECT ...) whose first value it is.

Triggers: CREATE RULE count_votes AFTER INSERT OR DELETE ON votes AS
  UPDATE features SET votes = (SELECT COUNT(*) FROM votes WHERE feature = :row.feature) WHERE id = :row.feature;
  It runs after each such write of that table with :row (the row as stored; the old row for a delete), :table and :event; give no "on" in the plan entry. Recompute a count or total with a SELECT rather than adding one, so deletes and edits stay right.

Schedules: a rule that runs on its own takes EVERY before AS: CREATE RULE morning_brief EVERY DAY AT '07:00' TZ 'Europe/Madrid' AS SEND EMAIL TO (SELECT email FROM readers) SUBJECT 'Your morning brief' ROWS (SELECT title, summary, link FROM stories WHERE days_between(published, :today) <= 1 ORDER BY published DESC LIMIT 15) NONE 'Nothing new today.';
  EVERY 30 MINUTES (15 at least) | EVERY HOUR | EVERY 6 HOURS | EVERY DAY AT 'HH:MM' | EVERY 3 DAYS AT 'HH:MM' | EVERY MONDAY AT 'HH:MM' | EVERY WEEKDAY AT 'HH:MM' | EVERY WEEK ON MONDAY, THURSDAY AT 'HH:MM'; then TZ 'Area/City' (UTC without it: give one whenever the request names a time of day). It runs with :today (the day in its TZ, 'YYYY-MM-DD') and :user null, once the owner switches schedules on. A schedule cannot also be a trigger.

Feeds and APIs that fill a table: a source is a rule that reads an RSS or Atom feed, or a JSON list, on a schedule and adds each new item as a row, once (KEY says what tells items apart):
CREATE SOURCE news FROM FEED 'https://example.com/rss.xml' EVERY 1 HOUR INTO stories (title, link, published, summary, image) KEY link;
CREATE SOURCE rates FROM JSON 'https://api.example.com/v1/rates' AT 'data.items' SECRET 'rates' EVERY DAY AT '06:00' INTO rates (currency = code, rate = value.mid) KEY currency;
  A feed item's fields are title, link, guid, published, updated (times), summary (the description as plain text), content (the full text, when the feed has it), author, categories and image; a column takes the field of its name, or column = field. A JSON source's fields are paths in each object (value.mid); AT is where the list is in the answer. SECRET names the API key its owner keeps (never write a key in the statement). LIMIT n reads the first n items (50 by default, 200 at most). The table needs those columns (TEXT for summary and content, TIMESTAMP for published); the owner allows the host.
  Inside a rule, PULL FEED | JSON 'url' (or :row.url) INTO t (...) KEY col [AS got] does the same, and gives {added, items}: FOR f IN (SELECT url FROM feeds) DO PULL FEED :f.url INTO stories (title, link, published, summary) KEY link; END; for many feeds kept in a table.
  What the model should do with each new item (a summary, a category) is a trigger AFTER INSERT ON stories, written in JavaScript with app.ask (only for what words need: SQL sorts, counts and dates).

- A rule with statements is one transaction: its SELECTs see the data as it was when it started (not its own writes: compute a total from the cart, not from the order lines it adds), its writes happen together at the end or not at all, and two runs never overlap, so REQUIRE really holds (nothing is sold twice). Triggers on its writes run after.
- :user.id is the signed-in person (also :user.name and :user.email; null when nobody is).
- A rule is checked when the version is built: the tables and columns it writes must exist.`;

// ---- a rule (JSON) as a statement

const sqlText = (v) => `'${String(v).replace(/'/g, "''")}'`;
const canon = (v) => (Array.isArray(v) ? v.map(canon) : v && typeof v === 'object'
  ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, canon(v[k])])) : v);
const same = (a, b) => JSON.stringify(canon(a)) === JSON.stringify(canon(b));
export const onOf = (on) => (on && on.table ? { table: on.table, event: [].concat(on.event || 'insert').length === 1 ? [].concat(on.event || 'insert')[0] : [].concat(on.event) } : null);

// A value as the statement says it: a parameter, a quoted literal, a number,
// or a SELECT in parentheses.
function valueSql(v) {
  if (v === null || v === undefined) return 'NULL';
  if (typeof v === 'number') return String(v);
  if (typeof v === 'boolean') return v ? 'TRUE' : 'FALSE';
  if (typeof v !== 'string') throw new RuleSqlError('a value that is not text, a number or a SELECT');
  if (PARAM_RE.test(v)) return v;
  if (SELECT_RE.test(v)) return `(${v})`;
  return sqlText(v);
}
const condSql = (c) => { if (typeof c !== 'string' || !SELECT_RE.test(c)) throw new RuleSqlError('a condition that is not a SELECT'); return `(${c})`; };

function stepSql(st, pad) {
  if (st.if !== undefined) { const { if: cond, ...rest } = st; return `IF ${condSql(cond)} THEN\n${pad}  ${stepSql(rest, `${pad}  `)};\n${pad}END`; }
  const as = st.as ? ` AS ${st.as}` : '';
  if (st.let !== undefined) return `LET ${st.let} = ${valueSql(st.value)}`;
  if (st.require !== undefined) return `REQUIRE ${condSql(st.require)}${st.else !== undefined ? ` ELSE ${sqlText(st.else)}` : ''}`;
  if (st.return !== undefined) return `RETURN ${valueSql(st.return)}`;
  if (st.insert !== undefined || st.upsert !== undefined) {
    const vals = Object.entries(st.values || {});
    if (!vals.length) throw new RuleSqlError('an insert with no values');
    const head = `${st.upsert !== undefined ? 'UPSERT' : 'INSERT'} INTO ${st.upsert ?? st.insert} (${vals.map(([k]) => k).join(', ')}) VALUES (${vals.map(([, v]) => valueSql(v)).join(', ')})`;
    return st.upsert !== undefined ? `${head} FIND (${st.find})${as}` : `${head}${as}`;
  }
  if (st.update !== undefined) {
    const id = valueSql(st.id);
    const own = (v) => { const m = typeof v === 'string' && v.match(/^SELECT ([\s\S]+) FROM (\S+) WHERE id = ([\s\S]+)$/); return m && m[2] === st.update && m[3] === id ? m[1] : null; };
    const sets = Object.entries(st.set || {}).map(([k, v]) => `${k} = ${own(v) ?? valueSql(v)}`);
    if (!sets.length) throw new RuleSqlError('an update that sets nothing');
    return `UPDATE ${st.update} SET ${sets.join(', ')} WHERE id = ${id}${as}`;
  }
  if (st.remove !== undefined) return `DELETE FROM ${st.remove} WHERE id = ${valueSql(st.id)}`;
  if (st.call !== undefined) return `CALL ${st.call}(${Object.entries(st.input || {}).map(([k, v]) => `${k} => ${valueSql(v)}`).join(', ')})${as}`;
  if (st.email !== undefined) {
    const e = st.email || {};
    return `SEND EMAIL TO ${valueSql(e.to)}${e.subject !== undefined ? ` SUBJECT ${sqlText(e.subject)}` : ''}${e.text !== undefined ? ` TEXT ${sqlText(e.text)}` : ''}${e.rows !== undefined ? ` ROWS (${e.rows})` : ''}${e.none !== undefined ? ` NONE ${sqlText(e.none)}` : ''}`;
  }
  if (st.pull !== undefined) return pullSql(st.pull, as);
  if (st.each !== undefined) return `FOR ${st.as} IN (${st.each}) DO\n${(st.do || []).map((x) => `${pad}  ${stepSql(x, `${pad}  `)};`).join('\n')}\n${pad}END`;
  throw new RuleSqlError(`a step of an unknown kind: ${Object.keys(st).join(', ')}`);
}

function pullSql(p, as = '') {
  const url = typeof p.url === 'string' && PARAM_RE.test(p.url) ? p.url : sqlText(p.url);
  const cols = Object.entries(p.cols || {}).map(([c, f]) => (c === f ? c : `${c} = ${f}`)).join(', ');
  return `PULL ${String(p.from || 'feed').toUpperCase()} ${url}${p.at ? ` AT ${sqlText(p.at)}` : ''}${p.secret ? ` SECRET ${sqlText(p.secret)}` : ''} INTO ${p.into} (${cols}) KEY ${p.key}${p.limit ? ` LIMIT ${p.limit}` : ''}${as}`;
}

// A rule (its JSON, trigger and schedule) as a CREATE RULE (or CREATE
// SOURCE) statement that compiles back into exactly that rule, or null.
export function ruleStatement(slug, rule, on, every = null, source = false, as = null) {
  try {
    const events = on && on.table ? ` AFTER ${[].concat(on.event || 'insert').map((e) => e.toUpperCase()).join(' OR ')} ON ${on.table}${as === 'app' ? ' RUNS AS APP' : ''}` : '';
    const when = every ? ` ${everyText(every)}` : '';
    let text;
    const one = rule && Array.isArray(rule.steps) && rule.steps.length === 1 ? rule.steps[0] : null;
    if (source && every && one && one.pull && !one.as && !one.if && !PARAM_RE.test(one.pull.url)) {
      const p = one.pull;
      const pull = pullSql(p);
      // PULL FEED 'url' [AT] [SECRET] INTO ... -> FROM FEED 'url' [AT] [SECRET] EVERY ... INTO ...
      const into = pull.indexOf(' INTO ');
      text = `CREATE SOURCE ${slug} FROM ${pull.slice(5, into)}${when} ${pull.slice(into + 1)}`;
      const back = compileRule(text);
      return same(back.rule, rule) && same(back.every, every) ? text : null;
    }
    if (rule && typeof rule.query === 'string') {
      if (events || when) return null;
      text = `CREATE RULE ${slug}${rule.one ? ' RETURNS ROW' : rule.value ? ' RETURNS VALUE' : ''} AS\n  ${rule.query}`;
    } else if (rule && Array.isArray(rule.steps) && rule.steps.length) {
      text = `CREATE RULE ${slug}${events}${when} AS\n${rule.steps.map((st) => `  ${stepSql(st, '  ')};`).join('\n')}`;
    } else return null;
    const back = compileRule(text);
    return same(back.rule, rule) && same(back.on, onOf(on)) && same(back.every || null, every || null) ? text : null;
  } catch (e) {
    if (e instanceof RuleSqlError) return null;
    throw e;
  }
}
