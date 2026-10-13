// Static mode for the Engram live inspector (engram/demo/index.html).
//
// The inspector normally talks to demo/server.py, which runs Claude Code and
// asks Engram's daemon to query its Strata store. On a static site there is
// no server, so this file answers the same HTTP calls in the browser:
//
// * /api/chat replays a recorded session (inspector-data.js), turn by turn,
//   with the records Engram wrote for each turn appearing as the turn runs.
// * inspect_query runs the SQL in a small JavaScript model of Strata over the
//   recorded records and their recorded vectors: the same analyser, BM25,
//   hash embedder, rank fusion, graph traversal and personalized PageRank.
// * timeline and inspect_layout return what the real daemon returned at the
//   end of the recording.
"use strict";
(() => {
const D = window.ENGRAM_DEMO;

// ====================================================================== text analysis (strata/src/text/analyzer.rs)
const STOPWORDS = new Set(["a","an","and","are","as","at","be","but","by","for","if","in","into","is","it","no","not","of","on","or","such","that","the","their","then","there","these","they","this","to","was","will","with"]);
const QUERY_STOPWORDS = new Set(["a","about","above","after","again","against","ain","all","am","an","and","any","are","aren","as","at","be","because","been","before","being","below","between","both","but","by","can","couldn","d","did","didn","do","does","doesn","doing","don","down","during","each","few","for","from","further","had","hadn","has","hasn","have","haven","having","he","her","here","hers","herself","him","himself","his","how","i","if","in","into","is","isn","it","its","itself","just","ll","m","ma","me","mightn","more","most","mustn","my","myself","needn","no","nor","not","now","o","of","off","on","once","only","or","other","our","ours","ourselves","out","over","own","re","s","same","shan","she","should","shouldn","so","some","such","t","than","that","the","their","theirs","them","themselves","then","there","these","they","this","those","through","to","too","under","until","up","ve","very","was","wasn","we","were","weren","what","when","where","which","while","who","whom","why","will","with","won","wouldn","y","you","your","yours","yourself","yourselves"]);
const FOLD = {"ß": "ss", "æ": "ae", "œ": "oe", "ø": "o", "đ": "d", "ł": "l", "ı": "i", "ĳ": "ij", "þ": "th", "ð": "d"};
const words = text => text.match(/[\p{L}\p{N}][\p{L}\p{N}̀-ͯ]*/gu) || [];
const fold = w => w.toLowerCase().normalize("NFD").replace(/[̀-ͯ]/g, "").replace(/[ßæœøđłıĳþð]/g, c => FOLD[c]);
// Porter (1980). Strata uses Snowball English (Porter 2), which differs on a few suffixes.
const stem = (() => {
  const step2 = {ational: "ate", tional: "tion", enci: "ence", anci: "ance", izer: "ize", bli: "ble", alli: "al", entli: "ent", eli: "e", ousli: "ous", ization: "ize", ation: "ate", ator: "ate", alism: "al", iveness: "ive", fulness: "ful", ousness: "ous", aliti: "al", iviti: "ive", biliti: "ble", logi: "log"};
  const step3 = {icate: "ic", ative: "", alize: "al", iciti: "ic", ical: "ic", ful: "", ness: ""};
  const c = "[^aeiou]", v = "[aeiouy]", C = c + "[^aeiouy]*", V = v + "[aeiou]*";
  const mgr0 = new RegExp("^(" + C + ")?" + V + C), meq1 = new RegExp("^(" + C + ")?" + V + C + "(" + V + ")?$");
  const mgr1 = new RegExp("^(" + C + ")?" + V + C + V + C), s_v = new RegExp("^(" + C + ")?" + v);
  return w => {
    if (w.length < 3 || /\d/.test(w)) return w;
    const first = w[0]; if (first === "y") w = "Y" + w.slice(1);
    let re, re2, re3, re4, fp, st;
    re = /^(.+?)(ss|i)es$/; re2 = /^(.+?)([^s])s$/;
    if (re.test(w)) w = w.replace(re, "$1$2"); else if (re2.test(w)) w = w.replace(re2, "$1$2");
    re = /^(.+?)eed$/; re2 = /^(.+?)(ed|ing)$/;
    if (re.test(w)) { fp = re.exec(w); if (mgr0.test(fp[1])) w = w.slice(0, -1); }
    else if (re2.test(w)) {
      fp = re2.exec(w); st = fp[1];
      if (s_v.test(st)) {
        w = st; re2 = /(at|bl|iz)$/; re3 = /([^aeiouylsz])\1$/; re4 = new RegExp("^" + C + v + "[^aeiouwxy]$");
        if (re2.test(w)) w += "e"; else if (re3.test(w)) w = w.slice(0, -1); else if (re4.test(w)) w += "e";
      }
    }
    re = /^(.+?)y$/; if (re.test(w)) { fp = re.exec(w); if (s_v.test(fp[1])) w = fp[1] + "i"; }
    re = /^(.+?)(ational|tional|enci|anci|izer|bli|alli|entli|eli|ousli|ization|ation|ator|alism|iveness|fulness|ousness|aliti|iviti|biliti|logi)$/;
    if (re.test(w)) { fp = re.exec(w); if (mgr0.test(fp[1])) w = fp[1] + step2[fp[2]]; }
    re = /^(.+?)(icate|ative|alize|iciti|ical|ful|ness)$/;
    if (re.test(w)) { fp = re.exec(w); if (mgr0.test(fp[1])) w = fp[1] + step3[fp[2]]; }
    re = /^(.+?)(al|ance|ence|er|ic|able|ible|ant|ement|ment|ent|ou|ism|ate|iti|ous|ive|ize)$/; re2 = /^(.+?)(s|t)(ion)$/;
    if (re.test(w)) { fp = re.exec(w); if (mgr1.test(fp[1])) w = fp[1]; }
    else if (re2.test(w)) { fp = re2.exec(w); st = fp[1] + fp[2]; if (mgr1.test(st)) w = st; }
    re = /^(.+?)e$/;
    if (re.test(w)) { fp = re.exec(w); st = fp[1]; re2 = new RegExp("^" + C + v + "[^aeiouwxy]$"); if (mgr1.test(st) || (meq1.test(st) && !re2.test(st))) w = st; }
    if (/ll$/.test(w) && mgr1.test(w)) w = w.slice(0, -1);
    if (first === "y") w = "y" + w.slice(1);
    return w;
  };
})();
const termOf = w => STOPWORDS.has(w) ? null : stem(w);
function analyze(text) { const out = []; for (const w of words(text || "")) { const t = termOf(fold(w)); if (t) out.push(t); } return out; }
function analyzeQuery(text, dropStop = true) {
  const out = [];
  for (const w0 of words(text)) { const w = fold(w0); if (dropStop && QUERY_STOPWORDS.has(w)) continue; const t = termOf(w); if (t && !out.includes(t)) out.push(t); }
  return out;
}

// ====================================================================== Engram's hash embedder (engram/src/embed.rs)
const DIM = D.dim, MASK64 = (1n << 64n) - 1n, enc = new TextEncoder();
function fnv(s) { let h = 0xcbf29ce484222325n; for (const b of enc.encode(s)) { h ^= BigInt(b); h = (h * 0x100000001b3n) & MASK64; } return h; }
function embed(text) {
  const v = new Array(DIM).fill(0);
  for (const w of String(text).split(/[^\p{L}\p{N}]+/u)) { if (!w) continue; const h = fnv(w.toLowerCase()); v[Number(h % BigInt(DIM))] += (h >> 63n) === 0n ? 1 : -1; }
  const n = Math.hypot(...v); return n > 0 ? v.map(x => x / n) : v;
}
const dot = (a, b) => { let s = 0; for (let i = 0; i < a.length; i++) s += a[i] * b[i]; return s; };

// ====================================================================== the table
// Rows revealed so far, in seq order, with their indexes rebuilt on change.
const COLS = ["id", "session", "kind", "tag", "turn", "seq", "ts", "importance", "files", "text", "ctx", "links", "emb"];
const FTYPE = {id: "KEYWORD", session: "KEYWORD", kind: "KEYWORD", tag: "KEYWORD", turn: "INT", seq: "INT", ts: "INT", importance: "FLOAT", files: "TEXT", text: "TEXT", ctx: "TEXT", body: "TEXT", emb: "VECTOR", links: "EDGES"};
const TEXTF = ["files", "body"];
const parseLinks = s => { try { return (JSON.parse(s || "[]") || []).map(e => Array.isArray(e) ? {label: e[0], to: e[1]} : {label: "", to: e}); } catch { return []; } };
let visible = 0, idx = null;
const ALL = D.records.map(r => ({...r, _links: parseLinks(r.links)}));
function table() {
  if (idx && idx.n === visible) return idx;
  const rows = ALL.slice(0, visible);
  const t = {n: visible, rows, pk: new Map(rows.map((r, i) => [r.id, i])), text: {}, inEdges: new Map()};
  for (const f of TEXTF) {
    const dict = new Map(), lens = [];
    rows.forEach((r, d) => {
      const terms = analyze(f === "body" ? (r.text || "") + "\n" + (r.ctx || "") : r.files);
      lens.push(terms.length);
      terms.forEach((term, pos) => {
        let pl = dict.get(term); if (!pl) dict.set(term, pl = []);
        let last = pl[pl.length - 1];
        if (!last || last.doc !== d) pl.push(last = {doc: d, tf: 0, pos: []});
        last.tf++; last.pos.push(pos);
      });
    });
    t.text[f] = {dict, lens, total: lens.reduce((a, b) => a + b, 0)};
  }
  rows.forEach(r => r._links.forEach(e => { if (!t.inEdges.has(e.to)) t.inEdges.set(e.to, []); t.inEdges.get(e.to).push({from: r.id, label: e.label}); }));
  return idx = t;
}

// ====================================================================== SQL (a subset of strata/src/query/sql.rs)
function lex(src) {
  const toks = []; let i = 0;
  const KW = new Set(["SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "IN", "BETWEEN", "GROUP", "BY", "ORDER", "ASC", "DESC", "LIMIT", "OFFSET", "MATCH", "REACHABLE", "PATH"]);
  while (i < src.length) {
    const c = src[i];
    if (/\s/.test(c)) { i++; continue; }
    if (c === "-" && src[i + 1] === "-") { while (i < src.length && src[i] !== "\n") i++; continue; }
    if (c === "'") { let j = i + 1, s = ""; while (j < src.length) { if (src[j] === "'" && src[j + 1] === "'") { s += "'"; j += 2; continue; } if (src[j] === "'") break; s += src[j++]; } if (j >= src.length) throw new Error("unterminated string"); toks.push({t: "str", v: s}); i = j + 1; continue; }
    if (/[0-9]/.test(c) || (c === "." && /[0-9]/.test(src[i + 1]))) { const m = /^[0-9]*\.?[0-9]+(e[-+]?[0-9]+)?/i.exec(src.slice(i)); toks.push({t: "num", v: parseFloat(m[0])}); i += m[0].length; continue; }
    if (c === ":" && /[A-Za-z_]/.test(src[i + 1])) { const m = /^:[A-Za-z_]\w*/.exec(src.slice(i)); toks.push({t: "param", v: m[0].slice(1)}); i += m[0].length; continue; }
    if (/[A-Za-z_]/.test(c)) { const m = /^[A-Za-z_]\w*/.exec(src.slice(i)); const up = m[0].toUpperCase(); toks.push(KW.has(up) ? {t: "kw", v: up} : {t: "id", v: m[0]}); i += m[0].length; continue; }
    const two = src.slice(i, i + 2);
    if (["=>", "<=", ">=", "!=", "<>"].includes(two)) { toks.push({t: "op", v: two === "<>" ? "!=" : two}); i += 2; continue; }
    if ("(),*=<>+-;".includes(c)) { toks.push({t: "op", v: c}); i++; continue; }
    throw new Error(`unexpected character '${c}'`);
  }
  toks.push({t: "eof"});
  return toks;
}
function parse(src) {
  const toks = lex(src); let p = 0;
  const peek = (o = 0) => toks[p + o];
  const is = (t, v) => peek().t === t && (v === undefined || peek().v === v);
  const isKw = v => is("kw", v), isOp = v => is("op", v);
  const take = () => toks[p++];
  const what = () => peek().v ?? "end of statement";
  const expect = (t, v) => { if (!is(t, v)) throw new Error(`expected ${v || t}, found ${what()}`); return take(); };
  const ident = () => { if (is("id")) return take().v; throw new Error(`expected a name, found ${what()}`); };
  function literal() {
    if (is("str") || is("num")) return {lit: take().v};
    if (is("param")) return {param: take().v};
    if (isOp("-") && peek(1).t === "num") { take(); return {lit: -take().v}; }
    throw new Error(`expected a value, found ${what()}`);
  }
  function namedArgs(args) { while (isOp(",")) { take(); const n = ident(); expect("op", "=>"); if (n === "through") args[n] = {expr: orExpr()}; else if (is("id")) args[n] = {lit: take().v}; else args[n] = literal(); } return args; }
  function seeds() {
    if (isOp("(") && peek(1).t === "kw" && peek(1).v === "SELECT") { take(); const q = select(); expect("op", ")"); return {sub: q}; }
    if (isOp("(")) { take(); const list = [literal()]; while (isOp(",")) { take(); list.push(literal()); } expect("op", ")"); return {list}; }
    return {list: [literal()]};
  }
  function select() {
    expect("kw", "SELECT");
    const cols = [];
    do {
      if (cols.length) take();
      if (isOp("*")) { take(); cols.push({star: true}); continue; }
      const n = ident();
      if (isOp("(")) { take(); if (isOp("*")) take(); expect("op", ")"); cols.push({fn: n.toLowerCase()}); }
      else cols.push({col: n});
    } while (isOp(","));
    expect("kw", "FROM"); const tbl = ident();
    if (tbl !== "memory") throw new Error(`unknown table ${tbl}`);
    let where = null, group = null, order = [], limit = null, offset = 0;
    if (isKw("WHERE")) { take(); where = orExpr(); }
    if (isKw("GROUP")) { take(); expect("kw", "BY"); group = ident(); }
    if (isKw("ORDER")) {
      take(); expect("kw", "BY");
      do { if (order.length) take(); const e = scoreExpr(); let dir = "DESC", explicit = false; if (isKw("ASC") || isKw("DESC")) { dir = take().v; explicit = true; } order.push({e, dir, explicit}); } while (isOp(","));
    }
    if (isKw("LIMIT")) { take(); limit = expect("num").v; if (isKw("OFFSET")) { take(); offset = expect("num").v; } }
    return {cols, where, group, order, limit, offset};
  }
  function orExpr() { let l = andExpr(); while (isKw("OR")) { take(); l = {op: "or", a: l, b: andExpr()}; } return l; }
  function andExpr() { let l = notExpr(); while (isKw("AND")) { take(); l = {op: "and", a: l, b: notExpr()}; } return l; }
  function notExpr() { if (isKw("NOT")) { take(); return {op: "not", a: notExpr()}; } return primary(); }
  function primary() {
    if (isOp("(")) { take(); const e = orExpr(); expect("op", ")"); return e; }
    if (isKw("MATCH")) { take(); expect("op", "("); const f = ident(); expect("op", ","); const q = literal(); expect("op", ")"); return {op: "match", f, q}; }
    if (isKw("REACHABLE")) { take(); expect("op", "("); const f = ident(); expect("op", ","); const s = seeds(); const args = namedArgs({}); expect("op", ")"); return {op: "reach", f, s, args}; }
    if (isKw("PATH")) { take(); expect("op", "("); const f = ident(); expect("op", ","); const a = seeds(); expect("op", ","); const b = seeds(); const args = namedArgs({}); expect("op", ")"); return {op: "path", f, a, b, args}; }
    const col = ident();
    if (isKw("NOT") && peek(1).v === "IN") { take(); take(); return {op: "not", a: inList(col)}; }
    if (isKw("IN")) { take(); return inList(col); }
    if (isKw("BETWEEN")) { take(); const lo = literal(); expect("kw", "AND"); const hi = literal(); return {op: "between", col, lo, hi}; }
    if (is("op") && ["=", "!=", "<", "<=", ">", ">="].includes(peek().v)) { const o = take().v; return {op: "cmp", col, o, v: literal()}; }
    throw new Error(`expected a condition after ${col}`);
  }
  function inList(col) { expect("op", "("); const vs = [literal()]; while (isOp(",")) { take(); vs.push(literal()); } expect("op", ")"); return {op: "in", col, vs}; }
  function scoreExpr() {
    const terms = [scoreTerm(1)];
    while (isOp("+") || isOp("-")) { const s = take().v === "-" ? -1 : 1; terms.push(scoreTerm(s)); }
    return terms.length === 1 && !terms[0].weighted && terms[0].w === 1 ? terms[0].e : {op: "sum", terms};
  }
  function scoreTerm(sign) { let w = sign, weighted = false; if (is("num") && peek(1).v === "*") { w *= take().v; take(); weighted = true; } return {w, weighted, e: scoreAtom()}; }
  function scoreAtom() {
    if (is("num")) return {op: "colpos", n: take().v};
    const n = ident().toLowerCase();
    if (!isOp("(")) return {op: "col", col: n};
    take();
    if (n === "hops" || n === "count" || n === "score") { if (isOp("*")) take(); expect("op", ")"); return {op: n}; }
    if (n === "bm25" || n === "similarity" || n === "distance") { const f = ident(); expect("op", ","); const q = literal(); const args = namedArgs({}); expect("op", ")"); return {op: n, f, q, args}; }
    if (n === "ppr") { const f = ident(); expect("op", ","); const s = seeds(); const args = namedArgs({}); expect("op", ")"); return {op: "ppr", f, s, args}; }
    if (n === "rrf") { const inputs = [scoreExpr()]; const args = {}; while (isOp(",")) { take(); if (is("id") && peek(1).v === "=>") { const k = ident(); take(); args[k] = literal(); } else inputs.push(scoreExpr()); } expect("op", ")"); return {op: "rrf", inputs, args}; }
    throw new Error(`unknown function ${n}()`);
  }
  const q = select();
  if (isOp(";")) take();
  if (!is("eof")) throw new Error(`unexpected ${what()} after the statement`);
  return q;
}

// ====================================================================== execution
function query(req) {
  const t0 = performance.now();
  const T = table();
  const params = {...(req.params || {})};
  for (const [k, v] of Object.entries(req.embed || {})) params[k] = embed(v);
  const plan = [];
  const ctx = {T, plan, log: s => plan.push(s), stats: {filter: T.n, scored: 0},
    val: x => { if (x.param !== undefined) { if (!(x.param in params)) throw new Error(`no value bound for :${x.param}`); return params[x.param]; } return x.lit; }};
  const q = parse(req.sql || "");
  ctx.log(`static model of Strata: ${T.n} rows in one segment`);
  const res = run(q, ctx);
  const maxDims = req.max_dims, maxRows = req.max_rows ?? 1000;
  const rows = res.rows.slice(0, maxRows).map(r => r.map(v => v && v.vec ? {dim: v.vec.length, values: maxDims ? v.vec.slice(0, maxDims) : v.vec} : v));
  return {columns: res.cols, rows, total_rows: res.rows.length, ms: performance.now() - t0, plan, filter_docs: ctx.stats.filter, scored: ctx.stats.scored};
}
function run(q, ctx) {
  const {T} = ctx;
  let pass = null;
  if (q.where) { pass = evalWhere(q.where, ctx); ctx.stats.filter = pass.size; ctx.log(`filter: ${pass.size} of ${T.n} rows pass WHERE`); }
  const passing = [...Array(T.n).keys()].filter(d => !pass || pass.has(d));
  const isCount = q.cols.length >= 1 && q.cols.some(c => c.fn === "count");
  if (q.group || isCount) {
    if (!q.group) return {cols: ["COUNT(*)"], rows: [[passing.length]]};
    if (FTYPE[q.group] !== "KEYWORD") throw new Error("GROUP BY takes one KEYWORD column");
    const counts = new Map();
    for (const d of passing) { const v = T.rows[d][q.group]; counts.set(v, (counts.get(v) || 0) + 1); }
    ctx.log(`facet: ${counts.size} values of ${q.group}`);
    let out = [...counts];
    const o = q.order[0];
    if (o && (o.e.op === "count" || (o.e.op === "colpos" && o.e.n === 2))) out.sort((a, b) => (o.explicit && o.dir === "ASC" ? 1 : -1) * (a[1] - b[1]) || (a[0] < b[0] ? -1 : 1));
    else out.sort((a, b) => (o && o.explicit && o.dir === "DESC" ? -1 : 1) * (a[0] < b[0] ? -1 : 1));
    if (q.limit != null) out = out.slice(q.offset, q.offset + q.limit);
    return {cols: [q.group, "COUNT(*)"], rows: out};
  }
  let ranked; const o = q.order[0];
  const key = d => T.rows[d].id;
  const tie = (a, b) => { const t = q.order[1]; if (t && t.e.op === "col") { const x = T.rows[a.d][t.e.col], y = T.rows[b.d][t.e.col]; return (t.explicit && t.dir === "DESC" ? -1 : 1) * (x < y ? -1 : x > y ? 1 : 0); } return a.d - b.d; };
  if (!o) ranked = passing.map(d => ({d, score: null}));
  else if (o.e.op === "col" || o.e.op === "hops") {
    if (o.e.op === "col" && !(o.e.col in FTYPE)) throw new Error(`unknown column ${o.e.col}`);
    const get = o.e.op === "hops" ? (d => ctx.hops?.get(key(d)) ?? Infinity) : (d => T.rows[d][o.e.col]);
    const dir = o.explicit ? o.dir : "ASC";
    ranked = passing.map(d => ({d, score: null, k: get(d)})).sort((a, b) => (dir === "ASC" ? 1 : -1) * (a.k < b.k ? -1 : a.k > b.k ? 1 : 0) || tie(a, b));
    ctx.log(`sort by ${o.e.op === "hops" ? "hops()" : o.e.col} ${dir}`);
  } else {
    const sc = score(o.e, passing, ctx);
    ctx.stats.scored = sc.size;
    const asc = o.e.op === "distance" ? !(o.explicit && o.dir === "DESC") : (o.explicit && o.dir === "ASC");
    ranked = passing.filter(d => sc.has(d)).map(d => ({d, score: sc.get(d)})).sort((a, b) => (asc ? 1 : -1) * (a.score - b.score) || tie(a, b));
  }
  if (q.limit != null) { ranked = ranked.slice(q.offset, q.offset + q.limit); ctx.log(`limit ${q.limit}${q.offset ? " offset " + q.offset : ""}`); }
  const cols = [];
  for (const c of q.cols) {
    if (c.star) cols.push(...COLS.filter(x => x !== "emb"));
    else if (c.fn) cols.push(c.fn + "()");
    else { if (!(c.col in FTYPE) || c.col === "body") throw new Error(`unknown or unstored column ${c.col}`); cols.push(c.col); }
  }
  return {cols, keys: ranked.map(r => key(r.d)), rows: ranked.map(({d, score}) => cols.map(c => {
    if (c === "score()") return score;
    if (c === "hops()") return ctx.hops?.get(key(d)) ?? null;
    if (c === "emb") return {vec: T.rows[d].emb};
    return T.rows[d][c];
  }))};
}
const all = T => new Set(Array.from({length: T.n}, (_, i) => i));
function evalWhere(e, ctx) {
  const {T, val, log} = ctx;
  switch (e.op) {
    case "and": { const a = evalWhere(e.a, ctx), b = evalWhere(e.b, ctx); return new Set([...a].filter(x => b.has(x))); }
    case "or": { const a = evalWhere(e.a, ctx), b = evalWhere(e.b, ctx); return new Set([...a, ...b]); }
    case "not": { const a = evalWhere(e.a, ctx); return new Set([...all(T)].filter(x => !a.has(x))); }
    case "match": {
      if (!TEXTF.includes(e.f)) throw new Error(`MATCH needs an indexed TEXT field (body or files), not ${e.f}`);
      const r = evalMatch(parseMatch(String(val(e.q))), T.text[e.f], T);
      log(`MATCH(${e.f}, '${val(e.q)}'): ${r.lists} posting list${r.lists === 1 ? "" : "s"} -> ${r.set.size} rows`);
      return r.set;
    }
    case "cmp": case "in": case "between": {
      const t = FTYPE[e.col];
      if (!t) throw new Error(`unknown column ${e.col}`);
      if (t === "TEXT" || t === "VECTOR" || t === "EDGES") throw new Error(`${e.col} is ${t}; it cannot be compared`);
      let test;
      if (e.op === "between") { const lo = val(e.lo), hi = val(e.hi); test = x => x >= lo && x <= hi; }
      else if (e.op === "in") { const vs = new Set(e.vs.map(val)); test = x => vs.has(x); }
      else { const v = val(e.v); test = {"=": x => x === v, "!=": x => x !== v, "<": x => x < v, "<=": x => x <= v, ">": x => x > v, ">=": x => x >= v}[e.o]; }
      const set = new Set(); T.rows.forEach((r, d) => { if (test(r[e.col])) set.add(d); });
      return set;
    }
    case "reach": case "path": {
      if (e.f !== "links") throw new Error(`${e.op === "reach" ? "REACHABLE" : "PATH"} follows the EDGES field links`);
      const dir = e.args.dir ? String(val(e.args.dir)) : "out";
      const labels = e.args.label ? new Set(String(val(e.args.label)).split(",").map(x => x.trim())) : null;
      const through = e.args.through ? e.args.through.expr : null;
      const ok = through ? evalWhere(through, {...ctx, log: () => {}}) : null;
      const pass = k => !ok || ok.has(T.pk.get(k));
      if (e.op === "reach") {
        const hi = e.args.hops ? val(e.args.hops) : 1, lo = e.args.min_hops !== undefined ? val(e.args.min_hops) : 1;
        const seeds = resolveSeeds(e.s, ctx);
        const dist = new Map(seeds.filter(k => T.pk.has(k)).map(k => [k, 0]));
        let frontier = [...dist.keys()];
        for (let h = 1; h <= hi && frontier.length; h++) {
          const next = [];
          for (const k of frontier) for (const n of neighbours(T, k, dir, labels)) if (!dist.has(n) && T.pk.has(n)) { dist.set(n, h); if (h === hi || pass(n)) next.push(n); }
          frontier = next;
        }
        const set = new Set(); for (const [k, h] of dist) if (h >= lo && h <= hi) set.add(T.pk.get(k));
        log(`REACHABLE(links, ${seeds.length} seed${seeds.length === 1 ? "" : "s"}, hops ${lo}..${hi}, dir ${dir}): ${set.size} rows`);
        if (!ctx.hops) ctx.hops = dist;
        return set;
      }
      const from = resolveSeeds(e.a, ctx).filter(k => T.pk.has(k)), to = new Set(resolveSeeds(e.b, ctx));
      const maxh = e.args.max_hops ? val(e.args.max_hops) : 6;
      const prev = new Map(), seen = new Set(from); let frontier = from.slice(), hit = from.find(k => to.has(k));
      for (let h = 1; h <= maxh && !hit && frontier.length; h++) {
        const next = [];
        for (const k of frontier) { for (const n of neighbours(T, k, dir, labels)) if (!seen.has(n) && T.pk.has(n) && (to.has(n) || pass(n))) { seen.add(n); prev.set(n, k); next.push(n); if (to.has(n)) { hit = n; break; } } if (hit) break; }
        frontier = next;
      }
      const path = new Map(); if (hit) { const chain = []; for (let k = hit; k !== undefined; k = prev.get(k)) chain.unshift(k); chain.forEach((k, i) => path.set(k, i)); }
      log(`PATH(links): ${hit ? path.size - 1 + " hops" : "no path"}`);
      ctx.hops = path;
      return new Set([...path.keys()].map(k => T.pk.get(k)));
    }
  }
  throw new Error("unsupported condition");
}
function neighbours(T, key, dir, labels) {
  const out = [];
  if (dir === "out" || dir === "both") { const d = T.pk.get(key); if (d !== undefined) for (const e of T.rows[d]._links) if (!labels || labels.has(e.label)) out.push(e.to); }
  if (dir === "in" || dir === "both") for (const e of T.inEdges.get(key) || []) if (!labels || labels.has(e.label)) out.push(e.from);
  return out;
}
function resolveSeeds(s, ctx) {
  if (s.list) return s.list.map(x => String(ctx.val(x)));
  const sub = run(s.sub, {...ctx, hops: null, log: x => ctx.log("  seeds: " + x)});
  return sub.keys;
}
function parseMatch(src) {
  const toks = []; const re = /\s*("([^"]*)"|\(|\)|-|[^\s()"]+)/gy; let m;
  while ((m = re.exec(src)) && m[0].length) toks.push(m[2] !== undefined ? {phrase: m[2]} : {w: m[1]});
  let p = 0; const peek = () => toks[p];
  function q() { let l = and(); while (peek() && peek().w === "OR") { p++; l = {or: [l, and()]}; } return l; }
  function and() { let l = unary(); while (peek() && peek().w !== "OR" && peek().w !== ")") { if (peek().w === "AND") p++; l = {and: [l, unary()]}; } return l; }
  function unary() { const t = peek(); if (!t) throw new Error("MATCH: incomplete query"); if (t.w === "NOT" || t.w === "-") { p++; return {not: unary()}; } return prim(); }
  function prim() {
    const t = toks[p++];
    if (t.w === "(") { const e = q(); if (!peek() || peek().w !== ")") throw new Error("MATCH: missing )"); p++; return e; }
    if (t.phrase !== undefined) return {phrase: analyze(t.phrase)};
    if (t.w.endsWith("*")) return {prefix: fold(t.w.slice(0, -1))};
    const terms = analyze(t.w);
    return terms.length === 0 ? {none: true} : terms.length > 1 ? {phrase: terms} : {term: terms[0]};
  }
  const e = q(); if (p < toks.length) throw new Error("MATCH: unexpected " + (toks[p].w || toks[p].phrase));
  return e;
}
function evalMatch(e, ix, T) {
  let lists = 0;
  const go = e => {
    if (e.none) return null;
    if (e.term !== undefined) { lists++; return new Set((ix.dict.get(e.term) || []).map(x => x.doc)); }
    if (e.prefix !== undefined) { const out = new Set(); for (const [t, pl] of ix.dict) if (t.startsWith(e.prefix)) { lists++; pl.forEach(x => out.add(x.doc)); } return out; }
    if (e.phrase !== undefined) {
      if (!e.phrase.length) return null;
      const pls = e.phrase.map(t => { lists++; return new Map((ix.dict.get(t) || []).map(x => [x.doc, x.pos])); });
      const out = new Set();
      for (const [doc, pos0] of pls[0]) if (pls.every(pl => pl.has(doc)) && pos0.some(p0 => pls.every((pl, i) => pl.get(doc).includes(p0 + i)))) out.add(doc);
      return out;
    }
    if (e.not) { const a = go(e.not); return a ? new Set([...all(T)].filter(x => !a.has(x))) : null; }
    if (e.and) { const a = go(e.and[0]), b = go(e.and[1]); if (!a) return b; if (!b) return a; return new Set([...a].filter(x => b.has(x))); }
    if (e.or) { const a = go(e.or[0]), b = go(e.or[1]); return new Set([...(a || []), ...(b || [])]); }
  };
  return {set: go(e) || new Set(), lists};
}
function score(e, rows, ctx) {
  const {T, val, log} = ctx;
  const out = new Map(), rowset = new Set(rows);
  switch (e.op) {
    case "bm25": {
      if (!TEXTF.includes(e.f)) throw new Error(`bm25 needs an indexed TEXT field (body or files), not ${e.f}`);
      const k1 = e.args.k1 ? val(e.args.k1) : 1.2, b = e.args.b ? val(e.args.b) : 0.75;
      const text = String(val(e.q)), terms = analyzeQuery(text, !(e.args.stopwords && val(e.args.stopwords) === 0));
      const ix = T.text[e.f], avgdl = T.n ? Math.max(ix.total / T.n, 1e-9) : 1;
      for (const t of terms) {
        const pl = ix.dict.get(t) || [], idf = Math.log(1 + (Math.max(T.n - pl.length, 0) + 0.5) / (pl.length + 0.5));
        for (const {doc, tf} of pl) if (rowset.has(doc)) out.set(doc, (out.get(doc) || 0) + idf * tf * (k1 + 1) / (tf + k1 * (1 - b + b * ix.lens[doc] / avgdl)));
      }
      log(`bm25(${e.f}): terms [${terms.join(", ")}] -> ${out.size} rows scored`);
      return out;
    }
    case "similarity": case "distance": {
      if (e.f !== "emb") throw new Error(`${e.op} needs the VECTOR field emb`);
      const x = val(e.q), qv = Array.isArray(x) ? x : embed(String(x));
      if (qv.length !== DIM) throw new Error(`a vector for emb needs ${DIM} dimensions, got ${qv.length}`);
      for (const d of rows) { const s = dot(T.rows[d].emb, qv); out.set(d, e.op === "distance" ? Math.sqrt(Math.max(0, 2 - 2 * s)) : s); }
      log(`${e.op}(emb): exact scan of ${rows.length} vectors`);
      return out;
    }
    case "ppr": {
      const seeds = resolveSeeds(e.s, ctx).filter(k => T.pk.has(k));
      const alpha = e.args.alpha ? val(e.args.alpha) : 0.15, eps = e.args.eps ? val(e.args.eps) : 1e-4, dir = e.args.dir ? String(val(e.args.dir)) : "both";
      const p = new Map(), r = new Map(seeds.map(k => [k, 1 / seeds.length])), nb = new Map();
      const ns = k => { if (!nb.has(k)) nb.set(k, neighbours(T, k, dir, null).filter(n => T.pk.has(n))); return nb.get(k); };
      const queue = [...seeds]; let pushes = 0;
      while (queue.length && pushes < 200000) {
        const u = queue.shift(), ru = r.get(u) || 0, nu = ns(u);
        if (ru < eps * Math.max(nu.length, 1)) continue;
        pushes++; p.set(u, (p.get(u) || 0) + alpha * ru); r.set(u, 0);
        for (const v of nu) { const nv = (r.get(v) || 0) + (1 - alpha) * ru / nu.length; r.set(v, nv); if (nv >= eps * Math.max(ns(v).length, 1)) queue.push(v); }
      }
      for (const d of rows) { const k = T.rows[d].id; if (p.has(k)) out.set(d, p.get(k)); }
      log(`ppr(links, ${seeds.length} seeds): ${pushes} pushes, ${out.size} rows with mass`);
      return out;
    }
    case "rrf": {
      const k = e.args.k ? val(e.args.k) : 60, depth = e.args.depth ? val(e.args.depth) : 100;
      for (const inp of e.inputs) {
        const sc = score(inp, rows, ctx), asc = inp.op === "distance";
        [...sc].sort((a, b) => (asc ? 1 : -1) * (a[1] - b[1])).slice(0, depth).forEach(([d], i) => out.set(d, (out.get(d) || 0) + 1 / (k + i + 1)));
      }
      log(`rrf(k ${k}) over ${e.inputs.length} rankings`);
      return out;
    }
    case "sum": {
      const parts = e.terms.map(t => ({t, sc: score(t.e, rows, ctx)}));
      const cand = new Set(); for (const {sc} of parts) [...sc].sort((a, b) => b[1] - a[1]).slice(0, 100).forEach(([d]) => cand.add(d));
      for (const {t, sc} of parts) {
        const vs = [...cand].map(d => sc.get(d) ?? 0), lo = Math.min(...vs), hi = Math.max(...vs);
        for (const d of cand) out.set(d, (out.get(d) || 0) + t.w * (hi > lo ? ((sc.get(d) ?? 0) - lo) / (hi - lo) : 0));
      }
      log(`weighted sum of ${e.terms.length} scores, min-max normalised over ${cand.size} candidates`);
      return out;
    }
  }
  throw new Error("ORDER BY takes a score (bm25, similarity, distance, rrf, ppr or a weighted sum), a column or hops()");
}

// ====================================================================== replay
// Each recorded turn knows which records (by seq) and daemon events (by
// index) it produced; they appear while its stream plays.
let logShown = 0, current = null, sessions = [], turnIdx = 0, busy = false;
const json = (body, status = 200) => new Response(JSON.stringify(body), {status, headers: {"Content-Type": "application/json"}});
const sleep = ms => new Promise(r => setTimeout(r, ms));
function stats() {
  const by = {}; for (const r of ALL.slice(0, visible)) by[r.kind] = (by[r.kind] || 0) + 1;
  return {records: visible, by_kind: by, embedder: D.layout.embedder, dim: DIM};
}
function revealTo(seq) { while (visible < ALL.length && ALL[visible].seq <= seq) visible++; }
function live(afterSeq, log) {
  const recs = ALL.slice(0, visible).filter(r => r.seq > afterSeq);
  const cols = COLS.filter(c => c !== "ctx" && c !== "emb");
  const events = D.events.slice(Math.min(log, logShown), logShown);
  return {records: {columns: cols, rows: recs.map(r => cols.map(c => r[c]))}, events, log_offset: logShown, busy};
}
function chatStream(turn) {
  const te = new TextEncoder();
  return new ReadableStream({async start(ctl) {
    busy = true;
    const evs = turn.events, n = evs.length;
    const emit = ev => ctl.enqueue(te.encode(JSON.stringify(ev) + "\n"));
    for (let i = 0; i < n; i++) {
      const ev = evs[i];
      if (ev.t === "init") { current = ev.session; if (!sessions.some(s => s.id === ev.session)) sessions.push({id: ev.session, started: Date.now() / 1000, title: turn.message.slice(0, 80)}); }
      // Records written during the turn appear in proportion to its progress.
      const frac = (i + 1) / n;
      revealTo(turn.seq0 + Math.floor((turn.seqTurnEnd - turn.seq0) * frac));
      logShown = Math.max(logShown, turn.log0 + Math.floor((turn.logTurnEnd - turn.log0) * frac));
      emit(ev);
      await sleep(ev.t === "delta" ? 25 : ev.t === "hook" ? 500 : ev.t.startsWith("tool") ? 450 : 120);
    }
    busy = false;
    ctl.close();
    // Then the distiller runs, after the Stop hook.
    await sleep(2500);
    revealTo(turn.seqEnd); logShown = Math.max(logShown, turn.logEnd);
  }});
}
const realFetch = window.fetch.bind(window);
window.fetch = async (url, opts = {}) => {
  const u = new URL(url, location.href);
  if (!u.pathname.includes("/api/")) return realFetch(url, opts);
  const path = u.pathname.slice(u.pathname.indexOf("/api/"));
  const body = opts.body ? JSON.parse(opts.body) : {};
  if (path === "/api/info") return json({project: D.info.project, store: D.info.store, model: D.info.model, distill_model: D.info.distill_model, sessions, current, busy, stats: stats()});
  if (path === "/api/live") return json(live(+u.searchParams.get("seq") || 0, +u.searchParams.get("log") || 0));
  if (path === "/api/session") { current = body.session || null; return json({current}); }
  if (path === "/api/stop") return json({ok: true});
  if (path === "/api/chat") {
    const turn = D.turns[turnIdx];
    if (busy) return json({error: "a turn is already running"}, 409);
    if (!turn) return json({error: "This copy of the inspector replays a recorded session, and all of it has played. Run engram/demo/run.sh to chat with Claude Code yourself."}, 400);
    if (body.message.trim() !== turn.message.trim()) return json({error: `This copy replays a recorded session, so it can only send the recorded messages. The next one is:\n\n${turn.message}\n\nClick it in the list above the box, or run engram/demo/run.sh to chat with Claude Code yourself.`}, 400);
    turnIdx++;
    return new Response(chatStream(turn), {headers: {"Content-Type": "application/x-ndjson"}});
  }
  if (path === "/api/rpc") {
    try {
      switch (body.op) {
        case "inspect_query": return json(query(body));
        case "inspect_embed": return json({embedder: D.layout.embedder, dim: DIM, values: embed(body.text || "")});
        case "inspect_layout": return json({...D.layout, live_docs: visible, note: "recorded at the end of the session"});
        case "stats": return json(stats());
        case "timeline": return json(D.timelines[body.id] ? {text: D.timelines[body.id]} : {error: "no timeline for " + body.id});
        case "search": return json({error: "Engram's own hybrid search runs in its daemon, which this static copy does not have. Use SQL mode: the reciprocal rank fusion example is close to what recall runs."});
      }
      return json({error: `op ${body.op} is not available here`}, 400);
    } catch (e) { return json({error: e.message}); }
  }
  return json({error: "not found"}, 404);
};

// Helpers the page uses to drive the replay.
window.ENGRAM_STATIC = {
  next: () => D.turns[turnIdx],
  finish() { visible = ALL.length; logShown = D.events.length; turnIdx = D.turns.length; sessions = D.info.sessions.slice(); current = sessions.at(-1)?.id || null; },
  analyze, embed,
};
})();
