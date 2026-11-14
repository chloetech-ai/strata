// Views and functions write SQL with the table names their app declared
// (`tasks`); in Strata the table is `app_<app>_tasks`. This rewrites the
// names in every table position (after FROM, JOIN and APPLY, after a comma
// in a FROM list, inside a parenthesised join), keeping the declared name as
// the alias so that `tasks.title` still reads, and refuses anything but one
// SELECT over the app's own tables.
//
// It is a boundary: a view's code is written by a model and may show content
// fetched from the web, and the engine holds a key that reads every table.
// So the text is split into tokens the way ChloeDB's parser (sqlparser 0.59,
// GenericDialect) splits it, and the forms whose ends that parser sees
// differently from a plain reading (block comments, E'' and r'' strings,
// $$ strings, a quoted or qualified table name, the |> pipe) are refused
// rather than modelled. The engine then asks the server which tables the
// rewritten query reads (`tablesOfPlan`), so a shape this misses still fails.

// Words sqlparser never takes as a table's alias without AS
// (keywords::RESERVED_FOR_TABLE_ALIAS).
const RESERVED_ALIAS = new Set(`WITH EXPLAIN ANALYZE SELECT WHERE GROUP SORT HAVING ORDER PIVOT UNPIVOT TOP LATERAL VIEW
  LIMIT OFFSET FETCH UNION EXCEPT INTERSECT MINUS ON JOIN INNER CROSS FULL LEFT RIGHT NATURAL USING CLUSTER DISTRIBUTE
  GLOBAL ANTI SEMI RETURNING ASOF MATCH_CONDITION OUTER SET QUALIFY WINDOW END FOR PARTITION PREWHERE SETTINGS FORMAT
  START CONNECT MATCH_RECOGNIZE SAMPLE TABLESAMPLE FROM OPEN`.split(/\s+/));

// Clause keywords that end a FROM list: a comma after one is not a table's.
const ENDS_FROM = new Set(`WHERE GROUP SORT HAVING ORDER LIMIT OFFSET FETCH UNION EXCEPT INTERSECT MINUS QUALIFY WINDOW
  PREWHERE START CONNECT SETTINGS FORMAT SELECT`.split(/\s+/));

// Words that write, change the server or name a table another way. Refused
// wherever they appear bare (a column of that name is written qualified,
// `t.update`, or quoted).
const FORBIDDEN = new Set(`TABLE INTO INSERT UPDATE DELETE MERGE UPSERT DROP CREATE ALTER TRUNCATE GRANT REVOKE COPY ATTACH
  DETACH PRAGMA CALL EXEC EXECUTE EXPLAIN DESCRIBE DESC_TABLE SHOW USE LOAD INSTALL VACUUM CHECKPOINT REPLACE_TABLE`.split(/\s+/));

// Functions whose arguments read `x FROM y` with no table: EXTRACT(dow FROM
// at), SUBSTRING(s FROM 2), TRIM(BOTH FROM s), OVERLAY(s PLACING t FROM 2).
const FROM_IN_ARGS = new Set(['EXTRACT', 'SUBSTRING', 'TRIM', 'OVERLAY']);

// What may come just before SELECT.
const BEFORE_SELECT = new Set(['UNION', 'INTERSECT', 'EXCEPT', 'MINUS', 'ALL', 'DISTINCT']);

const IDENT_START = /[\p{Alphabetic}_]/u;
const IDENT_PART = /[\p{Alphabetic}0-9@$#_]/u;
const ALNUM = /[\p{Alphabetic}\p{N}_]/u;

function refuse(msg) { throw new Error(`app.sql: ${msg}`); }

// Splits SQL into tokens: {k, v, s, e} with k one of ws, comment, word,
// qident, string, number, param, punct. Throws on a form it does not model.
export function tokenize(sql) {
  const out = [];
  let i = 0;
  const n = sql.length;
  const push = (k, s, e) => out.push({ k, v: sql.slice(s, e), s, e });
  const quoted = (s, q, k) => {
    let j = s + 1;
    for (;;) {
      if (j >= n) refuse(`a ${k === 'string' ? 'string' : 'quoted name'} is not closed`);
      if (sql[j] === q) {
        if (sql[j + 1] === q) { j += 2; continue; }
        return j + 1;
      }
      j++;
    }
  };
  while (i < n) {
    const c = sql[i];
    const next = sql[i + 1];
    if (/\s/.test(c)) {
      let j = i + 1;
      while (j < n && /\s/.test(sql[j])) j++;
      push('ws', i, j); i = j; continue;
    }
    if (c === '-' && next === '-') {
      let j = sql.indexOf('\n', i);
      if (j < 0) j = n;
      push('comment', i, j); i = j; continue;
    }
    if (c === '/' && next === '*') refuse('block comments (/* */) are not allowed; use -- comments');
    if (c === "'") { const j = quoted(i, "'", 'string'); push('string', i, j); i = j; continue; }
    if (c === '"' || c === '`') { const j = quoted(i, c, 'qident'); push('qident', i, j); i = j; continue; }
    // A letter that sqlparser reads as a string's prefix: b'', r'', n'',
    // e'' (backslash escapes), x'', u&''.
    if (/[bBrRnNeExX]/.test(c) && (next === "'" || (/[bBrR]/.test(c) && next === '"'))) {
      refuse(`prefixed strings (${c}'...') are not allowed`);
    }
    if (/[uU]/.test(c) && next === '&' && sql[i + 2] === "'") refuse("U&'...' strings are not allowed");
    if (IDENT_START.test(c) || ((c === '@' || c === '#') && next !== undefined && /[\p{Alphabetic}_@#]/u.test(next))) {
      let j = i + 1;
      while (j < n && IDENT_PART.test(sql[j])) j++;
      push('word', i, j); i = j; continue;
    }
    if (/[0-9]/.test(c) || (c === '.' && next !== '_')) {
      // As sqlparser: digits, 0x and hex digits, one '.', digits, an
      // exponent only when a digit follows it, then an optional L.
      let j = i;
      while (j < n && /[0-9]/.test(sql[j])) j++;
      if (sql.slice(i, j) === '0' && sql[j] === 'x') {
        j++;
        while (j < n && /[0-9a-fA-F]/.test(sql[j])) j++;
        push('number', i, j); i = j; continue;
      }
      if (sql[j] === '.') j++;
      while (j < n && /[0-9]/.test(sql[j])) j++;
      if (j - i === 1 && c === '.') { push('punct', i, j); i = j; continue; }
      if (sql[j] === 'e' || sql[j] === 'E') {
        let k = j + 1;
        if (sql[k] === '+' || sql[k] === '-') k++;
        if (/[0-9]/.test(sql[k] || '')) { j = k; while (j < n && /[0-9]/.test(sql[j])) j++; }
      }
      if (sql[j] === 'L') j++;
      push('number', i, j); i = j; continue;
    }
    if (c === '$') {
      let j = i + 1;
      while (j < n && ALNUM.test(sql[j])) j++;
      if (sql[j] === '$') refuse('$$ strings are not allowed');
      push('param', i, j); i = j; continue;
    }
    if (c === ':') {
      if (next === ':' || next === '=') { push('punct', i, i + 2); i += 2; continue; }
      if (next !== undefined && /[\p{Alphabetic}_0-9]/u.test(next)) {
        let j = i + 1;
        while (j < n && IDENT_PART.test(sql[j])) j++;
        push('param', i, j); i = j; continue;
      }
    }
    if (c === '|' && next === '>') refuse('the |> pipe is not allowed');
    if (c === '\\') refuse('a backslash outside a string is not allowed');
    push('punct', i, i + 1); i++;
  }
  return out;
}

// The library's table functions (engine/sqlfns.mjs), called in FROM as
// tables are (FROM amortization(:p, :r, :n)): they are Strata's, not the
// app's, so their names stay as written.
const tableFunctions = new Set();
export const useTableFunctions = (names) => { tableFunctions.clear(); for (const n of names) tableFunctions.add(String(n).toLowerCase()); };

// scope(name), when given, is asked for each of the app's tables the query
// reads: null (every row), or {sql(alias, {left})} giving the condition its
// rows must meet (access.mjs, a table declared for each person's or
// company's own rows); it may throw to refuse the table. The condition joins
// the WHERE of each SELECT that reads the table, so search, ranking and
// graph walks keep working on the table's own scan.
export function rewriteSql(sql, tables, physical, { scope = null } = {}) {
  const text = String(sql || '').trim();
  if (!text) throw new Error('empty SQL');
  const all = tokenize(text);
  const toks = all.filter((t) => t.k !== 'ws' && t.k !== 'comment');
  if (toks.length && toks[toks.length - 1].v === ';') toks.pop();
  if (!toks.length) throw new Error('empty SQL');
  if (toks.some((t) => t.v === ';')) throw new Error('one statement at a time');
  const up = (t) => (t && t.k === 'word' ? t.v.toUpperCase() : null);
  const first = toks[0];
  if (!['SELECT', 'WITH'].includes(up(first)) && first.v !== '(') {
    throw new Error('app.sql runs SELECT queries only; write rows with app.insert, app.update and app.remove');
  }

  const names = new Set(tables.map((t) => String(t).toLowerCase()));
  const used = new Set();
  const edits = []; // {s, e, text}
  // A frame per open parenthesis: the function it calls (if any), whether
  // its query has a FROM list open, the CTEs its WITH defined.
  const frames = [{ fn: null, from: false, select: false, ctes: new Set(), with: false, factor: false, block: null }];
  const top = () => frames[frames.length - 1];
  const isCte = (name) => frames.some((f) => f.ctes.has(name));
  const skip = new Set(); // token indexes that are names, not keywords
  const noTable = (name) => {
    const list = [...names].join(', ') || 'none yet';
    throw new Error(`no table ${name} in this app (its tables: ${list})`);
  };

  // A table's alias: AS and any word, or a word sqlparser takes as one.
  // Answers the alias as written, or null.
  const alias = (j) => {
    const t = toks[j];
    if (up(t) === 'AS') { if (toks[j + 1]) skip.add(j + 1); return toks[j + 1] ? toks[j + 1].v : null; }
    if (t && (t.k === 'qident' || (t.k === 'word' && !RESERVED_ALIAS.has(up(t))))) { skip.add(j); return t.v; }
    return null;
  };
  // The SELECT a table factor belongs to: the innermost frame's query (a
  // parenthesised join has none of its own).
  const blockOf = () => { for (let k = frames.length - 1; k >= 0; k--) if (frames[k].block) return frames[k].block; return null; };
  // Once a SELECT's clauses are read: its scoped tables' conditions join its
  // WHERE (or make one).
  const finish = (b, last) => {
    if (!b || !b.scoped.length) return;
    if (b.outer) refuse('a RIGHT or FULL JOIN cannot read a table kept to each person\'s own rows; use JOIN or LEFT JOIN');
    const cond = b.scoped.join(' AND ');
    if (b.where !== null) {
      edits.push({ s: toks[b.where].e, e: toks[b.where].e, text: ` ${cond} AND (` });
      const at = toks[b.whereEnd ?? last].e;
      edits.push({ s: at, e: at, text: ')' });
    } else {
      const at = toks[b.fromEnd ?? last].e;
      edits.push({ s: at, e: at, text: ` WHERE ${cond}` });
    }
  };
  const CLAUSE_END = new Set(['GROUP', 'HAVING', 'ORDER', 'LIMIT', 'OFFSET', 'FETCH', 'QUALIFY', 'WINDOW', 'SORT', 'SETTINGS', 'FORMAT']);

  // The table factor at toks[j]: a plain app table (rewritten), a CTE, one
  // of the library's table functions, or a parenthesis (a subquery or a
  // join, which the frame then reads).
  const factor = (j) => {
    let t = toks[j];
    if (up(t) === 'LATERAL') { j++; t = toks[j]; if (!t || t.v !== '(') refuse('LATERAL takes a subquery'); }
    if (!t) refuse('a table is missing after FROM or JOIN');
    if (t.v === '(') { toks[j].factor = true; return; }
    if (t.k === 'qident') refuse(`write table names without quotes (${t.v})`);
    if (t.k !== 'word') refuse(`expected a table name, found ${t.v}`);
    skip.add(j);
    const name = t.v.toLowerCase();
    const after = toks[j + 1];
    if (after && after.v === '.') refuse(`write the app's table names alone, not ${t.v}.${toks[j + 2]?.v ?? ''}`);
    if (after && after.v === '(') {
      if (!tableFunctions.has(name)) refuse(`no table function ${t.v}`);
      return;
    }
    if (isCte(name)) { alias(j + 1); return; }
    if (!names.has(name)) noTable(t.v);
    used.add(name);
    const had = alias(j + 1);
    edits.push({ s: t.s, e: t.e, text: `${physical(name)}${had ? '' : ` AS ${name}`}` });
    const sc = scope ? scope(name) : null;
    if (sc) {
      const b = blockOf();
      if (!b) refuse(`${t.v} is read outside a SELECT`);
      const left = up(toks[j - 1]) === 'JOIN' && (up(toks[j - 2]) === 'LEFT' || (up(toks[j - 2]) === 'OUTER' && up(toks[j - 3]) === 'LEFT'));
      b.scoped.push(sc.sql(had || name, { left }));
    }
  };

  // One CTE's head at toks[j]: name [(cols)] AS [[NOT] MATERIALIZED] (
  const cte = (j) => {
    const t = toks[j];
    if (!t || t.k !== 'word') refuse('a WITH query needs a plain name');
    skip.add(j);
    top().ctes.add(t.v.toLowerCase());
  };

  for (let i = 0; i < toks.length; i++) {
    const t = toks[i];
    const prev = toks[i - 1];
    const f = top();
    if (t.v === '(') {
      frames.push({ fn: prev && prev.k === 'word' && !skip.has(i - 1) ? up(prev) : null, from: !!t.factor, select: false, ctes: new Set(), with: false, factor: !!t.factor, block: null });
      const nx = toks[i + 1];
      if (t.factor && nx && !['SELECT', 'WITH', 'VALUES'].includes(up(nx)) && nx.v !== '(') factor(i + 1);
      if (t.factor && up(nx) === 'VALUES') top().from = false;
      continue;
    }
    if (t.v === ')') {
      if (frames.length === 1) refuse('a ) closes nothing');
      const closed = frames.pop();
      finish(closed.block, i - 1);
      if (closed.factor) alias(i + 1);
      continue;
    }
    if (t.v === ',') {
      if (f.with && !f.select) { cte(i + 1); continue; }
      if (f.from) factor(i + 1);
      continue;
    }
    if (t.k !== 'word' || skip.has(i) || (prev && prev.v === '.')) continue;
    const w = up(t);
    if (FORBIDDEN.has(w)) refuse(`${w} is not allowed in app.sql (it runs one SELECT)`);
    if (w === 'AS') { if (toks[i + 1]) skip.add(i + 1); continue; }
    if (w === 'SELECT') {
      if (prev && prev.v !== '(' && prev.v !== ')' && !BEFORE_SELECT.has(up(prev))) refuse('SELECT can start a query only');
      f.select = true; f.from = false; f.with = false;
      finish(f.block, i - 1);
      f.block = { scoped: [], where: null, whereEnd: null, fromEnd: null, outer: false };
      continue;
    }
    if (w === 'WITH') {
      if (prev && prev.v !== '(') refuse('WITH can start a query only');
      f.with = true;
      let j = i + 1;
      if (up(toks[j]) === 'RECURSIVE') j++;
      cte(j);
      continue;
    }
    if (w === 'FROM') {
      const p1 = up(prev), p2 = up(toks[i - 2]), p3 = up(toks[i - 3]);
      const distinct = p1 === 'DISTINCT' && (p2 === 'IS' || (p2 === 'NOT' && p3 === 'IS'));
      if (distinct || (FROM_IN_ARGS.has(f.fn) && !f.select)) continue;
      f.from = true;
      factor(i + 1);
      continue;
    }
    if (w === 'JOIN' || w === 'APPLY') { f.from = true; factor(i + 1); continue; }
    if ((w === 'RIGHT' || w === 'FULL') && blockOf()) blockOf().outer = true;
    if (f.block) {
      const b = f.block;
      if (w === 'WHERE' && b.where === null) b.where = i;
      else if (CLAUSE_END.has(w)) {
        if (b.where !== null && b.whereEnd === null) b.whereEnd = i - 1;
        if (b.where === null && b.fromEnd === null) b.fromEnd = i - 1;
      } else if (['UNION', 'INTERSECT', 'EXCEPT', 'MINUS'].includes(w)) { finish(b, i - 1); f.block = null; }
    }
    if (ENDS_FROM.has(w)) { f.from = false; continue; }
    // Graph walks may name a links table as their first argument.
    if ((w === 'REACHABLE' || w === 'PATH' || w === 'PPR') && toks[i + 1]?.v === '(') {
      const a = toks[i + 2];
      if (a && a.k === 'word' && names.has(a.v.toLowerCase()) && toks[i + 3]?.v !== '.') {
        const name = a.v.toLowerCase();
        if (scope && scope(name)) refuse(`${a.v} is kept to each person's own rows: it cannot be walked as a graph`);
        used.add(name);
        skip.add(i + 2);
        edits.push({ s: a.s, e: a.e, text: physical(name) });
      }
    }
  }
  if (frames.length !== 1) refuse('a ( is not closed');
  finish(frames[0].block, toks.length - 1);

  // The text as written, with the table names replaced; a trailing ; goes.
  let end = toks[toks.length - 1].e;
  let out = '';
  let at = 0;
  for (const e of edits.sort((a, b) => a.s - b.s)) { out += text.slice(at, e.s) + e.text; at = e.e; }
  const lastAll = all.filter((t) => t.k !== 'ws');
  if (lastAll.length && lastAll[lastAll.length - 1].k === 'comment') end = text.length;
  out += text.slice(at, end);
  return { sql: out, tables: [...used] };
}

// Every table a plan reads (its JSON, as /v1/explain gives it in "ir"):
// each Search's `table`, a walk's edge table, and those of its subqueries.
export function tablesOfPlan(ir) {
  const out = new Set();
  const walk = (v) => {
    if (Array.isArray(v)) { for (const x of v) walk(x); return; }
    if (!v || typeof v !== 'object') return;
    for (const [k, x] of Object.entries(v)) {
      if ((k === 'table' || k === 'edges') && typeof x === 'string') out.add(x);
      else walk(x);
    }
  };
  walk(ir);
  return out;
}

// Tables the library's SQL functions read in their bodies (sa_numbers): a
// plan that calls one reads them, and they hold nothing of anyone's.
const functionReads = new Set();
export const functionReadTables = () => [...functionReads];
export const useFunctionReads = (bodies) => {
  functionReads.clear();
  for (const b of bodies) for (const m of String(b || '').matchAll(/\b(?:FROM|JOIN)\s+([A-Za-z_][A-Za-z0-9_]*)/gi)) functionReads.add(m[1].toLowerCase());
};

// Asks the server which tables `sql` reads (POST /v1/explain plans it with
// the catalog it runs on) and refuses one outside `allowed`, so that a shape
// rewriteSql lets through still cannot read another app's rows. A verdict is
// kept per text: tables do not depend on the parameters' values.
const verdicts = new Map();
export async function checkReads(db, sql, params, allowed) {
  const key = `${[...allowed].sort().join(',')}\n${sql}`;
  if (verdicts.has(key)) return;
  const out = await db.explain(sql, params || {});
  const ok = new Set([...allowed].map((t) => t.toLowerCase()));
  for (const t of tablesOfPlan(out && out.ir)) {
    const lower = t.toLowerCase();
    if (!ok.has(lower) && !functionReads.has(lower)) refuse(`the query reads ${t}, which is not one of this app's tables`);
  }
  if (verdicts.size > 2000) verdicts.clear();
  verdicts.set(key, true);
}
