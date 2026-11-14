// Every piece of an app is kept in Strata as SQL: the statement that makes
// it. A table is its CREATE TABLE, a screen its CREATE SCREEN, a rule its
// CREATE RULE; a piece written in JavaScript (a function, a code view, a
// component, a helper) is a CREATE ... LANGUAGE javascript AS $$ ... $$, and
// a pattern a CREATE PATTERN ... LANGUAGE json. The compiled form (a screen's
// JSON with its patterns expanded, a rule's steps) is made when the piece is
// read, never stored: a better pattern reaches every screen that uses it.
//
// toSource(piece) gives the statement for a piece as the engine holds it;
// fromSource(row) gives back {code, sql, statement}: the form the engine
// runs, the statement as stored, and (for screens and rules written as
// statements) the statement people and the model edit.
import { compileStatement, screenStatement, sameScreen } from './statement.mjs';
import { compileRule, ruleStatement, onOf } from './rulesql.mjs';
import { everyText } from './schedule.mjs';
import { expandScreen } from './patterns.mjs';
import { colSql } from './types.mjs';

const sqlText = (s) => `'${String(s).replace(/'/g, "''")}'`;
const oneLine = (s) => String(s || '').replace(/\s+/g, ' ').trim();
const canon = (v) => (Array.isArray(v) ? v.map(canon) : v && typeof v === 'object'
  ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, canon(v[k])])) : v);
const same = (a, b) => JSON.stringify(canon(a)) === JSON.stringify(canon(b));
const parse = (s) => { try { return JSON.parse(s); } catch { return undefined; } };

// A statement starts with CREATE (after -- comments).
const HEAD = /^\s*(?:--[^\n]*\n\s*)*CREATE\s+(?:OR\s+REPLACE\s+)?(TABLE|SCREEN|RULE|SOURCE|FUNCTION|COMPONENT|HELPER|PATTERN)\b/i;
export const isSource = (code) => typeof code === 'string' && HEAD.test(code);

// A body in dollar quotes, with a tag the body does not hold.
function wrap(head, language, body) {
  let tag = '$$';
  for (let i = 0; String(body).includes(tag); i++) tag = `$${['js', 'body', 'src'][i] || `q${i}`}$`;
  return `${head} LANGUAGE ${language} AS ${tag}\n${body}\n${tag}`;
}
const WRAPPED = /^\s*(?:--[^\n]*\n\s*)*CREATE\s+(?:OR\s+REPLACE\s+)?(SCREEN|RULE|FUNCTION|COMPONENT|HELPER|PATTERN)\s+[\s\S]*?\bLANGUAGE\s+(javascript|json)\s+AS\s+(\$[A-Za-z0-9_]*\$)\n([\s\S]*)\n\3\s*;?\s*$/;

const events = (on, every = null, as = null) => `${on && on.table ? ` AFTER ${[].concat(on.event || 'insert').map((e) => String(e).toUpperCase()).join(' OR ')} ON ${on.table}${as === 'app' ? ' RUNS AS APP' : ''}` : ''}${every ? ` ${everyText(every)}` : ''}`;
const titled = (slug, title) => `${slug}${title && title !== slug ? ` TITLE ${sqlText(title)}` : ''}`;

// A table as the CREATE TABLE that makes it (with the app's own name for it;
// in Strata it is app_<app>_<name>).
export function tableStatement(spec) {
  const defs = ['id KEYWORD PRIMARY KEY'];
  if (spec.links) defs.push(`src KEYWORD NOT NULL REFERENCES ${spec.links}`, `dst KEYWORD NOT NULL REFERENCES ${spec.links}`);
  for (const c of spec.columns || []) if (!(spec.links && (c.name === 'src' || c.name === 'dst'))) defs.push(colSql(c));
  defs.push('created TIMESTAMP DEFAULT CURRENT_TIMESTAMP');
  // Its declared access (access.mjs) follows it, one ALLOW statement each.
  const access = Array.isArray(spec.access) && spec.access.length ? `;\n${spec.access.join(';\n')}` : '';
  return `${spec.description ? `-- ${oneLine(spec.description)}\n` : ''}CREATE TABLE ${spec.name} (\n  ${defs.join(',\n  ')}\n)${spec.links ? " OPTIONS(edges = 'src, dst')" : ''}${access}`;
}

// The photos a screen shows: placed after its patterns are expanded (a
// block names a photo, "photo": "a sushi counter", and the build finds one),
// so a statement carries them in a PHOTOS part, in the order they show.
const photoSlot = (v) => v && typeof v === 'object' && !Array.isArray(v) && typeof v.photo === 'string' && v.photo.trim() && !v.photo.startsWith('=') && (v.src === undefined || v.src === null || v.src === '');
const PHOTO_KEYS = ['src', 'credit', 'credit_url'];

// Puts the photos found into a screen (in place), each into the next block
// that names the same photo and has none yet.
export function withPhotos(screen, photos) {
  const list = [...(photos || [])];
  const walk = (v) => {
    if (!list.length || !v || typeof v !== 'object') return;
    if (Array.isArray(v)) { for (const x of v) walk(x); return; }
    if (photoSlot(v) && list[0].photo === v.photo) { const p = list.shift(); for (const k of PHOTO_KEYS) if (p[k] != null) v[k] = p[k]; }
    for (const x of Object.values(v)) if (x && typeof x === 'object') walk(x);
  };
  walk(screen.body || []);
  return screen;
}

// The photos placed in a screen (shown) that its statement (base) leaves to be found.
export function photosOf(base, shown) {
  const out = [];
  const walk = (a, b) => {
    if (!a || !b || typeof a !== 'object' || typeof b !== 'object') return;
    if (Array.isArray(a)) { if (Array.isArray(b)) a.forEach((x, i) => walk(x, b[i])); return; }
    if (photoSlot(a) && typeof b.src === 'string' && b.src) out.push({ photo: a.photo, ...Object.fromEntries(PHOTO_KEYS.filter((k) => b[k] != null).map((k) => [k, b[k]])) });
    for (const [k, x] of Object.entries(a)) if (x && typeof x === 'object') walk(x, b[k]);
  };
  walk(base.body || [], shown.body || []);
  return out;
}

const PHOTOS_LINE = /\n?^PHOTOS\s+\[[\s\S]*?^\](?=\s*$|\n)/m;
// A statement without its PHOTOS part (what the model edits: the photos are
// found again for a screen it changes).
export const withoutPhotos = (statement) => String(statement).replace(PHOTOS_LINE, '').trim();
const photosPart = (photos) => `PHOTOS [\n${photos.map((p) => `  ${JSON.stringify(p)}`).join(',\n')}\n]`;

// A screen statement compiled, its patterns expanded and its photos put in:
// the JSON it runs.
export function screenOf(statement) {
  const out = compileStatement(statement);
  const ex = expandScreen(out.screen);
  if (ex.problems.length) throw new Error(`${out.slug}: ${ex.problems.slice(0, 3).join('; ')}`);
  return withPhotos(ex.screen, out.photos);
}

// The statement of a screen (JSON as it runs): the one it was written as,
// with the photos found since, when that still makes exactly this screen (a
// repair patched into the JSON makes it stale); else one made from the JSON.
function screenSource(slug, title, screen, statement) {
  if (typeof statement === 'string') {
    try {
      const bare = withoutPhotos(statement);
      const base = screenOf(bare);
      const photos = photosOf(base, screen);
      const text = photos.length ? `${bare}\n${photosPart(photos)}` : bare;
      if (sameScreen(photos.length ? screenOf(text) : base, screen)) return text;
    } catch { /* stale */ }
  }
  return screenStatement(slug, title, screen);
}

// The statement for a piece, from what the engine holds: {kind, slug, title,
// code (the form it runs), spec (with statement, when it was written as one)}.
export function toSource({ kind, slug, title, code, spec = {} }) {
  if (isSource(code)) return code;
  const body = String(code ?? '');
  if (kind === 'table') return tableStatement({ name: slug, ...spec });
  if (kind === 'view' && spec.screen) {
    const screen = parse(body);
    return (screen && screenSource(slug, title, screen, spec.statement)) || wrap(`CREATE SCREEN ${titled(slug, title)}`, 'json', body);
  }
  if (kind === 'view') return wrap(`CREATE SCREEN ${titled(slug, title)}`, 'javascript', body);
  if (kind === 'function' && spec.rule) {
    const rule = parse(body);
    if (typeof spec.statement === 'string' && rule) {
      try { const back = compileRule(spec.statement); if (same(back.rule, rule) && same(back.on, onOf(spec.on)) && same(back.every || null, spec.every || null) && (back.as || null) === (spec.as || null)) return spec.statement.trim(); } catch { /* stale */ }
    }
    return (rule && ruleStatement(slug, rule, spec.on, spec.every, spec.source, spec.as)) || wrap(`CREATE RULE ${slug}${events(spec.on, spec.every, spec.as)}`, 'json', body);
  }
  if (kind === 'function') return wrap(`CREATE FUNCTION ${slug}${events(spec.on, spec.every, spec.as)}`, 'javascript', body);
  if (kind === 'component') return wrap(`CREATE COMPONENT ${slug}`, 'javascript', body);
  if (kind === 'helper') return wrap(`CREATE HELPER ${slug}`, 'javascript', body);
  if (kind === 'pattern') return wrap(`CREATE PATTERN ${slug}`, 'json', body);
  return body;
}

// The spec stored beside a piece's SQL: the statement a screen or rule was
// written as is that SQL, so the spec does not keep it; a screen or rule
// whose SQL was made from its JSON (written as JSON, or patched since) is
// marked derived, and is shown and changed as JSON, as it was written.
export function storedSpec(kind, spec, sql) {
  const out = { ...(spec || {}) };
  const written = typeof out.statement === 'string' ? out.statement : null;
  delete out.statement;
  delete out.derived;
  if (!isSource(sql)) return out;
  if ((kind === 'view' && out.screen) || (kind === 'function' && out.rule)) {
    const asWritten = written != null && (kind === 'view' ? withoutPhotos(sql) === withoutPhotos(written) : sql.trim() === written.trim());
    if (!asWritten && !WRAPPED.test(sql)) out.derived = true;
  }
  return out;
}

// What a stored row runs as: {code, sql, statement}. A row stored before
// pieces were kept as SQL runs as it is, and its SQL is made for showing.
export function fromSource({ kind, slug, title, code, spec = {} }) {
  if (!isSource(code)) {
    let sql = null;
    try { sql = toSource({ kind, slug, title, code, spec }); } catch { /* shown without */ }
    return { code: code ?? '', sql, statement: typeof spec.statement === 'string' ? spec.statement : null };
  }
  const w = code.match(WRAPPED);
  if (w) return { code: w[4], sql: code, statement: null };
  const what = code.match(HEAD)[1].toUpperCase();
  if (what === 'TABLE') return { code: '', sql: code, statement: null };
  if (what === 'SCREEN') return { code: JSON.stringify(screenOf(code), null, 1), sql: code, statement: spec.derived ? null : withoutPhotos(code) };
  if (what === 'RULE' || what === 'SOURCE') return { code: JSON.stringify(compileRule(code).rule, null, 1), sql: code, statement: spec.derived ? null : code };
  return { code, sql: code, statement: null };
}

// The whole app as one SQL script: its tables, screens, rules and functions,
// and the shared pieces it uses, each one's SQL as stored.
export function appScript(view) {
  const app = view.app || {};
  const one = (c) => `${c.description && c.kind !== 'table' ? `-- ${oneLine(c.description)}\n` : ''}${String(c.sql || toSource(c)).trim().replace(/;\s*$/, '')};`;
  const part = (title, caps) => (caps?.length ? [`-- ---- ${title}`, ...caps.map(one)].join('\n\n') : null);
  const views = [...(view.views || [])].sort((a, b) => (a.spec?.order ?? 50) - (b.spec?.order ?? 50));
  const fns = view.functions || [];
  const head = [
    `-- ${oneLine(app.name || app.id)}`,
    app.description ? `-- ${oneLine(app.description)}` : null,
    view.comp?.n ? `-- Version ${view.comp.n}${view.comp.summary ? `: ${oneLine(view.comp.summary)}` : ''}` : null,
    `-- Each piece is a row of sa_capabilities in ChloeDB, stored as this SQL; each table is app_${app.id}_<name>.`,
  ].filter(Boolean).join('\n');
  return `${[head, part('Tables', view.tables), part('Screens', views), part('Rules', fns.filter((f) => f.spec?.rule)), part('Functions', fns.filter((f) => !f.spec?.rule)),
    part('Shared components', view.components), part('Shared helpers', view.helpers)].filter(Boolean).join('\n\n')}\n`;
}
