// Everything the engine knows lives in Strata: apps, the capabilities they are
// made of (code included), each app's versions (compositions), the requests
// that changed them, what people did, and the suggestions made to them. Each
// app's own data lives in tables named app_<app>_<table>.

import { randomBytes } from 'node:crypto';
import { GRAPH_SQL, appDocs, capDoc, appNode, parseLinks } from './graph.mjs';
import { Walker, isWalkRefused } from './walk.mjs';

// Column types, the embedding model and a column's SQL live in types.mjs
// (source.mjs writes tables as CREATE TABLE with them too).
import { TYPES, EMBED_MODEL, colSql } from './types.mjs';
import { toSource, fromSource, isSource, storedSpec } from './source.mjs';
import { patternsGen, usePatterns } from './patterns.mjs';
import { sameScreen } from './statement.mjs';
export { TYPES, EMBED_MODEL };

const SYSTEM_TABLES = {
  sa_apps: `CREATE TABLE sa_apps (id KEYWORD PRIMARY KEY, name TEXT, description TEXT, head INT DEFAULT 0, ux TEXT, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  sa_capabilities: `CREATE TABLE sa_capabilities (id KEYWORD PRIMARY KEY, slug KEYWORD NOT NULL, version INT NOT NULL,
    kind KEYWORD NOT NULL, app KEYWORD, title TEXT, description TEXT, code TEXT, spec TEXT,
    forked_from KEYWORD, author KEYWORD, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  sa_compositions: `CREATE TABLE sa_compositions (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, n INT NOT NULL,
    caps KEYWORD[], request TEXT, summary TEXT, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  sa_requests: `CREATE TABLE sa_requests (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, text TEXT, source KEYWORD,
    status KEYWORD, comp INT, summary TEXT, log TEXT, error TEXT, ms INT, cost FLOAT, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  // Each build's timeline (trace.mjs), by request id.
  sa_traces: `CREATE TABLE sa_traces (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, trace TEXT, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  // What people did: who (a person, or a visitor's cookie) and on which
  // version of the app, so a change can be judged by what people do after it.
  // What people do in an app, one row per thing done: the kind (open, leave,
  // action, abandon, insert, update, delete, call, error), where (the browser
  // tab's session, the screen) and what (the table and row written, the fields
  // changed, the words searched for and how many rows they found, how long).
  sa_events: `CREATE TABLE sa_events (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, kind KEYWORD, cap KEYWORD,
    detail TEXT, person KEYWORD, v INT, session KEYWORD, screen KEYWORD, tbl KEYWORD, rid KEYWORD, fields KEYWORD,
    q KEYWORD, results INT, ms INT, label TEXT, taps INT, ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  // A suggestion from use names the pattern of use it answers (signal) and
  // what should change in people's use if it works (measure, JSON).
  sa_suggestions: `CREATE TABLE sa_suggestions (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, text TEXT, why TEXT,
    source KEYWORD, cap KEYWORD, status KEYWORD, signal KEYWORD, measure TEXT, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  // Changes the engine built on its own from how people use an app
  // (evolve.mjs): a version beside the live one, to try, keep or drop.
  sa_candidates: `CREATE TABLE sa_candidates (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, text TEXT, why TEXT, signal KEYWORD,
    measure TEXT, suggestion KEYWORD, request KEYWORD, base INT, n INT, kept INT, status KEYWORD, summary TEXT, problems TEXT,
    cost FLOAT, ms INT, decided TIMESTAMP, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  // What apps asked a model (reach.mjs app.ask), by "<app>:<hash of the
  // question>": the kept answer, given again for the same question.
  sa_asks: `CREATE TABLE sa_asks (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, kind KEYWORD, prompt TEXT, answer TEXT,
    cost FLOAT, ms INT, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  // Per-app settings, by "<app>:<name>".
  sa_settings: `CREATE TABLE sa_settings (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, name KEYWORD, value TEXT)`,
  // Apps, their pieces and the shared library as a graph (graph.mjs).
  sa_graph: GRAPH_SQL,
  // What every app gets (services.mjs): people and their sessions, who
  // belongs to which app, uploaded files (their bytes sit next to the
  // database) and the email apps send.
  sa_people: `CREATE TABLE sa_people (id KEYWORD PRIMARY KEY, email KEYWORD, name TEXT, hash KEYWORD, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  sa_members: `CREATE TABLE sa_members (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, person KEYWORD NOT NULL, role KEYWORD, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  // Companies (workspaces) of an app's people, and who works in which with
  // what role (admin or member): kept by the engine, never by an app's code,
  // so a row scope by :company_id (access.mjs) means what the engine says.
  sa_companies: `CREATE TABLE sa_companies (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, name TEXT, author KEYWORD, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  sa_memberships: `CREATE TABLE sa_memberships (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, company KEYWORD NOT NULL, person KEYWORD NOT NULL, role KEYWORD, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  sa_sessions: `CREATE TABLE sa_sessions (id KEYWORD PRIMARY KEY, person KEYWORD NOT NULL, expires TIMESTAMP, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  sa_files: `CREATE TABLE sa_files (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, name TEXT, type KEYWORD, size INT, author KEYWORD, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  // Each app's three designs (designs.mjs) and the one in use.
  sa_designs: `CREATE TABLE sa_designs (id KEYWORD PRIMARY KEY, designs TEXT, chosen INT, source KEYWORD, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
  sa_outbox: `CREATE TABLE sa_outbox (id KEYWORD PRIMARY KEY, app KEYWORD NOT NULL, recipients KEYWORD[], subject TEXT, body TEXT,
    status KEYWORD, error TEXT, author KEYWORD, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP)`,
};

// Columns added to system tables after they were first made: a store made
// before them gets them on start.
// The UX brief kept with an app (sa_apps.ux), or null.
export function briefOf(app) {
  if (!app?.ux) return null;
  try { const b = typeof app.ux === 'string' ? JSON.parse(app.ux) : app.ux; return b && typeof b === 'object' && b.job ? b : null; } catch { return null; }
}

const ADDED_COLUMNS = {
  // The UX brief, as JSON: who, job, often, device, first, glance.
  sa_apps: ['ux TEXT'],
  sa_events: ['person KEYWORD', 'v INT', 'session KEYWORD', 'screen KEYWORD', 'tbl KEYWORD', 'rid KEYWORD', 'fields KEYWORD', 'q KEYWORD', 'results INT', 'ms INT', 'label TEXT', 'taps INT'],
  sa_suggestions: ['signal KEYWORD', 'measure TEXT'],
  // What a piece is and does, embedded, so that pieces are found by meaning
  // as well as by their words (searchCapabilities). A store whose Strata
  // cannot load the model searches by words alone.
  sa_capabilities: [`meaning VECTOR OPTIONS(embed = 'title, description', model = '${EMBED_MODEL}')`],
};
// Below this cosine a piece found only by meaning is too far to show.
export const MEANING_MIN = Number(process.env.STRATA_APPS_MEANING_MIN || 0.2);

export const newId = (n = 8) => randomBytes(n).toString('base64url').replace(/[^a-zA-Z0-9]/g, '').slice(0, n).toLowerCase().padEnd(n, '0');
export const physical = (app, table) => `app_${app}_${table}`;
export const NAME_RE = /^[a-z][a-z0-9_]{0,30}$/;
export const SLUG_RE = /^[a-z][a-z0-9-]{0,40}$/;
// Components and helpers are shared by every app: they belong to the library,
// not to an app. Code calls them as ui.<name>(props) and lib.<name>.fn(...),
// so their names are JavaScript names.
export const LIB = 'lib';
export const COMPONENT_RE = /^[a-z][a-zA-Z0-9]{1,40}$/;
export const SHARED_KINDS = ['component', 'helper'];

const tableName = (t) => t.table || t.schema?.table || t.name;
const viewOf = (app, comp, caps) => ({
  app, comp, caps,
  tables: caps.filter((c) => c.kind === 'table'),
  views: caps.filter((c) => c.kind === 'view'),
  functions: caps.filter((c) => c.kind === 'function'),
  components: caps.filter((c) => c.kind === 'component'),
  helpers: caps.filter((c) => c.kind === 'helper'),
  shared: caps.filter((c) => SHARED_KINDS.includes(c.kind)),
});
const parseSpec = (s) => { try { return s ? JSON.parse(s) : {}; } catch { return {}; } };
const canon = (v) => (Array.isArray(v) ? v.map(canon) : v && typeof v === 'object'
  ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, canon(v[k])])) : v);
const sameScreenJson = (a, b) => { try { return sameScreen(JSON.parse(a), JSON.parse(b)); } catch { return false; } };
const sameJson = (a, b) => { try { return JSON.stringify(canon(JSON.parse(a))) === JSON.stringify(canon(JSON.parse(b))); } catch { return false; } };

const GRAPH_COLS = 'id, kind, app, slug, title, description, live, version, links';
// A node as the graph's REACHABLE queries above select it.
const nodeRow = ({ id, kind, app, slug, title, hops }) => ({ id, kind, app, slug, title, hops });

// A short name for an app from its description, until its first plan names
// it (and for good when that build fails): the words before a colon or a
// dash, else the first clause, without "A"/"An"/"The" in front, cut at a
// word, at most 40 characters. "A tiny reading list: links with a title"
// is "Tiny reading list".
export function shortName(description) {
  const first = String(description || '').trim().split(/[.\n!?]/)[0];
  let s = (first.split(/\s*[:;–—]\s*|\s+-\s+/)[0] || first).split(/,\s*/)[0].trim();
  if (s.length < 3) s = first.trim();
  s = s.replace(/^(an?|the|my|our)\s+/i, '');
  if (s.length > 40) { const cut = s.slice(0, 41); s = /\s/.test(cut) ? cut.replace(/\s+\S*$/, '') : s.slice(0, 40); }
  for (let k = 0; k < 3; k++) s = s.replace(/[\s,;:–—-]+$/, '').replace(/\s+(and|or|with|of|for|the|an?|to|in|on|my|at|by)$/i, '');
  return s ? s[0].toUpperCase() + s.slice(1) : 'New app';
}

// What an app with no version yet is, from its last request: building
// while it is queued or running, failed once it failed or was stopped, or
// when it still says running long after any build could (the engine stopped
// in the middle of it).
export function firstBuildStatus(last, now = Date.now(), staleMs = 30 * 60 * 1000) {
  if (!last) return 'building';
  if (last.status === 'failed' || last.status === 'stopped') return 'failed';
  if ((last.status === 'queued' || last.status === 'running') && now - new Date(last.created).getTime() > staleMs) return 'failed';
  return 'building';
}

export class Store {
  constructor(strata, log = () => {}) {
    this.db = strata;
    this.log = log;
    this.resolved = new Map(); // app -> {head, view}
    this.resolvedAt = new Map(); // "app#n" -> view of a version that is not the head (versions never change)
    this.capCache = new Map();  // capability id -> row as stored, its code SQL (capabilities never change once written)
    this.made = new Map();      // capability id -> {gen, row}: the row as it runs, compiled with the patterns of gen
    this.meaning = false;       // whether sa_capabilities has its meaning column
  }

  async init() {
    const have = new Set((await this.db.tables()).map(tableName).filter(Boolean));
    for (const [name, sql] of Object.entries(SYSTEM_TABLES)) {
      if (!have.has(name)) {
        await this.db.sql(sql);
        this.log(`created ${name}`);
      }
    }
    const tables = await this.db.tables();
    for (const [name, cols] of Object.entries(ADDED_COLUMNS)) {
      const fields = new Set((tables.find((t) => tableName(t) === name)?.schema?.fields || []).map((f) => f.name));
      for (const c of cols) {
        const col = c.split(' ')[0];
        if (fields.has(col)) { if (col === 'meaning') this.meaning = true; continue; }
        try {
          await this.db.sql(`ALTER TABLE ${name} ADD COLUMN ${c}`);
          if (col === 'meaning') this.meaning = true;
          this.log(`added ${col} to ${name}`);
        } catch (e) {
          if (col !== 'meaning') throw e;
          this.log(`pieces are searched by their words only: ${e.message}`);
        }
      }
    }
    // Screens are compiled with the patterns stored here (built in and added
    // by apps), so those come first; then every piece stored before pieces
    // were kept as SQL is written as its statement.
    await this.usePatterns();
    await this.storeAsSql();
    await this.rebuildGraph();
  }

  // The patterns stored in Strata become the ones screens expand from; a
  // screen compiled with others is compiled again when next read.
  async usePatterns() {
    const n = usePatterns(await this.patterns());
    this.resolved.clear();
    this.resolvedAt.clear();
    return n;
  }

  // Rewrites each piece whose code is not yet its SQL (stored by an engine
  // before pieces were kept as SQL) as its statement, in place: the same id,
  // the same piece, only written as SQL. A piece whose statement does not
  // give back exactly what it runs is left as it is.
  async storeAsSql() {
    if (await this.setting('_engine', 'stored-as-sql')) return;
    const rows = [];
    try {
      // By app and kind (a query returns at most 10000 rows), and a large
      // group by ranges of versions.
      const COLS = 'id, slug, version, kind, app, title, description, code, spec, forked_from, author, created';
      const groups = await this.db.query('SELECT app, kind, COUNT(*) AS n, MAX(version) AS top FROM sa_capabilities GROUP BY app, kind LIMIT 10000');
      for (const g of groups) {
        const where = 'app = :a AND kind = :k';
        if (Number(g.n) <= 10000) { rows.push(...await this.db.query(`SELECT ${COLS} FROM sa_capabilities WHERE ${where} LIMIT 10000`, { a: g.app, k: g.kind })); continue; }
        for (let lo = 0; lo <= Number(g.top); lo += 50) {
          rows.push(...await this.db.query(`SELECT ${COLS} FROM sa_capabilities WHERE ${where} AND version >= :lo AND version < :hi LIMIT 10000`, { a: g.app, k: g.kind, lo, hi: lo + 50 }));
        }
      }
    } catch (e) { this.log(`pieces as SQL: ${e.message}`); return; }
    const ops = [];
    let kept = 0;
    for (const r of rows) {
      if (isSource(r.code)) continue;
      const spec = parseSpec(r.spec);
      try {
        const sql = toSource({ ...r, spec });
        const back = fromSource({ ...r, code: sql, spec: storedSpec(r.kind, spec, sql) });
        const same = r.kind === 'table' || (r.kind === 'view' && spec.screen ? sameScreenJson(back.code, r.code) : r.kind === 'function' && spec.rule ? sameJson(back.code, r.code) : back.code === (r.code ?? ''));
        if (!isSource(sql) || !same) { kept++; continue; }
        const stored = storedSpec(r.kind, spec, sql);
        const { meaning, score, ...doc } = r;
        ops.push({ op: 'upsert', table: 'sa_capabilities', doc: { ...doc, code: sql, spec: JSON.stringify(stored) } });
      } catch (e) { kept++; this.log(`pieces as SQL: ${r.id}: ${e.message}`); }
    }
    for (let i = 0; i < ops.length; i += 200) await this.db.write(ops.slice(i, i + 200));
    this.capCache.clear();
    this.made.clear();
    if (ops.length || kept) this.log(`pieces as SQL: ${ops.length} rewritten as statements${kept ? `, ${kept} left as they were` : ''}`);
    await this.putSetting('_engine', 'stored-as-sql', new Date().toISOString());
  }

  async tableNames() {
    const tables = await this.db.tables();
    return tables.map(tableName).filter(Boolean);
  }

  // ---- apps

  async listApps() {
    return this.db.query('SELECT id, name, description, head, created FROM sa_apps ORDER BY created DESC LIMIT 200');
  }

  async getApp(id) {
    const [row] = await this.db.get('sa_apps', [id]);
    return row || null;
  }

  async createApp({ name, description }) {
    const id = newId(6);
    await this.db.write([{ op: 'insert', table: 'sa_apps', doc: { id, name: name || 'New app', description: description || '', head: 0 } }]);
    await this.syncAppGraph(id);
    return id;
  }

  async updateApp(id, fields) {
    const app = await this.getApp(id);
    if (!app) throw new Error(`no app ${id}`);
    const doc = { ...app, ...fields };
    delete doc.created;
    await this.db.write([{ op: 'upsert', table: 'sa_apps', doc: { ...doc, created: app.created } }]);
    this.resolved.delete(id);
    await this.syncAppGraph(id);
  }

  // Deletes an app: its own tables (app_<id>_*, and no other app's), and
  // every row the engine keeps of it (its pieces, versions, requests,
  // timelines, use, suggestions, settings, members, files, mail and graph).
  // The shared library and other apps' pieces stay. Returns {tables, rows}.
  async deleteApp(id) {
    if (!/^[a-z0-9]{6}$/.test(String(id))) throw new Error(`no app ${id}`);
    const prefix = `app_${id}_`;
    const tables = (await this.db.tables()).map(tableName).filter((t) => t && t.startsWith(prefix));
    // A links table refers to the table it links: dropped first.
    const dropped = [];
    for (let pass = 0; pass < 3 && dropped.length < tables.length; pass++) {
      for (const t of tables) {
        if (dropped.includes(t)) continue;
        try { await this.db.sql(`DROP TABLE ${t}`); dropped.push(t); } catch (e) { if (pass === 2) throw e; }
      }
    }
    let rows = 0;
    for (const table of ['sa_capabilities', 'sa_compositions', 'sa_requests', 'sa_traces', 'sa_events', 'sa_suggestions', 'sa_candidates', 'sa_asks', 'sa_settings', 'sa_graph', 'sa_members', 'sa_companies', 'sa_memberships', 'sa_files', 'sa_outbox']) {
      for (;;) {
        let ids;
        try { ids = await this.db.query(`SELECT id FROM ${table} WHERE app = :a LIMIT 1000`, { a: id }); } catch { break; }
        if (!ids.length) break;
        await this.db.write(ids.map((r) => ({ op: 'delete', table, key: r.id })));
        rows += ids.length;
        if (ids.length < 1000) break;
      }
    }
    await this.db.write([{ op: 'delete', table: 'sa_designs', key: id }, { op: 'delete', table: 'sa_apps', key: id }]);
    this.resolved.delete(id);
    this.resolvedAt.clear();
    this.capCache.clear();
    this.made.clear();
    this.walkers?.clear();
    return { tables: dropped, rows };
  }

  // A copy of an app's live version as a new app: its own pieces copied as
  // they are stored (the same SQL, under the new app), the shared ones it
  // pins pinned again, its tables made, its look kept, and (rows) its rows
  // copied, up to 10000 a table. Returns the new app's id.
  async duplicateApp(id, { name, rows = true } = {}) {
    const view = await this.resolve(id);
    const copy = await this.createApp({ name: String(name || `${view.app.name} (copy)`).slice(0, 60), description: view.app.description || '' });
    if (view.app.ux) await this.updateApp(copy, { ux: view.app.ux });
    const stored = await this.db.get('sa_capabilities', view.comp.caps);
    const ops = [];
    const caps = [];
    for (const r of stored) {
      if (!r) continue;
      if (r.app !== id) { caps.push(r.id); continue; }
      const { meaning, score, created, ...doc } = r;
      const cid = `${copy}/${r.kind}/${r.slug}@1`;
      ops.push({ op: 'insert', table: 'sa_capabilities', doc: { ...doc, id: cid, app: copy, version: 1 } });
      caps.push(cid);
    }
    for (let i = 0; i < ops.length; i += 200) await this.db.write(ops.slice(i, i + 200));
    // A links table refers to the table it links: made after it.
    const existing = new Set(await this.tableNames());
    const tables = [...view.tables].sort((x, y) => Number(!!x.spec.links) - Number(!!y.spec.links));
    for (const t of tables) await this.ensureTable(copy, t.spec, existing);
    if (rows) {
      for (const t of tables) {
        const vec = new Set((t.spec.columns || []).filter((c) => c.type === 'meaning').map((c) => c.name));
        const got = await this.db.query(`SELECT * FROM ${physical(id, t.spec.name)} LIMIT 10000`).catch(() => []);
        const docs = got.map((r) => Object.fromEntries(Object.entries(r).filter(([k, v]) => !vec.has(k) && k !== 'score' && v !== null)));
        for (let i = 0; i < docs.length; i += 500) await this.db.write(docs.slice(i, i + 500).map((doc) => ({ op: 'insert', table: physical(copy, t.spec.name), doc })));
      }
    }
    const designs = await this.designs(id);
    if (designs) await this.putDesigns(copy, designs);
    await this.writeComposition(copy, caps, `A copy of ${view.app.name}`, `A copy of ${view.app.name} (version ${view.comp.n})`);
    return copy;
  }

  // An app's rows, every table (up to 10000 rows each), for its owner to
  // keep: {app, exported, tables: {name: [rows]}}.
  async exportApp(id) {
    const view = await this.resolve(id);
    const tables = {};
    for (const t of view.tables) {
      const vec = new Set((t.spec.columns || []).filter((c) => c.type === 'meaning').map((c) => c.name));
      const got = await this.db.query(`SELECT * FROM ${physical(id, t.spec.name)} ORDER BY created LIMIT 10000`).catch(() => []);
      tables[t.spec.name] = got.map((r) => Object.fromEntries(Object.entries(r).filter(([k]) => !vec.has(k) && k !== 'score')));
    }
    return { app: { id, name: view.app.name, version: view.comp.n }, exported: new Date().toISOString(), tables };
  }

  // What each app's builds cost (the model, as sa_requests records it):
  // Map app -> {usd, builds}; for one app with app.
  async costs(app = null) {
    const rows = await this.db.query(`SELECT app, SUM(cost) AS usd, COUNT(*) AS n FROM sa_requests${app ? ' WHERE app = :a' : ''} GROUP BY app LIMIT 5000`, app ? { a: app } : {}).catch(() => []);
    return new Map(rows.map((r) => [r.app, { usd: Math.round((Number(r.usd) || 0) * 100) / 100, builds: Number(r.n) || 0 }]));
  }

  // ---- designs: three per app, by the app's id

  async designs(app) {
    const [row] = await this.db.get('sa_designs', [app]);
    if (!row) return null;
    try {
      const designs = JSON.parse(row.designs);
      return Array.isArray(designs) && designs.length ? { designs, chosen: Math.min(Math.max(Number(row.chosen) || 0, 0), designs.length - 1), source: row.source || '' } : null;
    } catch { return null; }
  }

  async putDesigns(app, { designs, chosen = 0, source = '' }) {
    await this.db.write([{ op: 'upsert', table: 'sa_designs', doc: { id: app, designs: JSON.stringify(designs), chosen, source } }]);
  }

  // ---- capabilities

  // Pieces by id, as they run: each one's SQL compiled (source.mjs), with
  // the SQL as `sql`.
  async getCapabilities(ids) {
    const missing = ids.filter((id) => !this.capCache.has(id));
    if (missing.length) {
      const rows = await this.db.get('sa_capabilities', missing);
      // The vector stays in Strata: nothing here reads it.
      rows.forEach((r, i) => { if (r) { const { meaning, ...rest } = r; this.capCache.set(missing[i], { ...rest, spec: parseSpec(r.spec) }); } });
    }
    return ids.map((id) => this.capCache.get(id)).filter(Boolean).map((r) => this.runs(r));
  }

  // A stored row as it runs. A screen is compiled with the patterns in use,
  // so it is compiled again once they change.
  runs(raw) {
    const hit = this.made.get(raw.id);
    const gen = patternsGen();
    if (hit && hit.gen === gen) return hit.row;
    let out;
    let failed = null;
    try { out = fromSource(raw); } catch (e) {
      failed = e.message;
      this.log(`${raw.id}: ${e.message}`);
      out = { code: raw.kind === 'view' && raw.spec?.screen ? JSON.stringify({ body: [{ h: 'This screen could not be built' }, { text: e.message, muted: true }] }) : '', sql: raw.code, statement: null };
    }
    const spec = { ...(raw.spec || {}) };
    if (out.statement) spec.statement = out.statement;
    const row = { ...raw, code: out.code, spec, sql: out.sql, ...(isSource(raw.code) ? {} : { legacy: true }) };
    if (!failed) this.made.set(raw.id, { gen, row });
    return row;
  }

  async nextVersion(app, kind, slug) {
    const rows = await this.db.query('SELECT version FROM sa_capabilities WHERE app = :a AND kind = :k AND slug = :s ORDER BY version DESC LIMIT 1', { a: app, k: kind, s: slug });
    return rows.length ? Number(rows[0].version) + 1 : 1;
  }

  // A capability is never changed once written: a change is a new version,
  // with its own id, `app/kind/slug@version`.
  async putCapability({ app, slug, kind, title, description, code, spec, forked_from, author }) {
    const version = await this.nextVersion(app, kind, slug);
    const id = `${app}/${kind}/${slug}@${version}`;
    // What is stored is the piece's SQL (source.mjs): a screen's or a
    // rule's statement, a table's CREATE TABLE, JavaScript inside a CREATE.
    // The statement a screen or rule was written as is that SQL, so the
    // spec does not keep it too.
    const sql = toSource({ kind, slug, title: title || slug, code: code || '', spec: spec || {} });
    const stored = storedSpec(kind, spec, sql);
    const doc = { id, slug, version, kind, app, title: title || slug, description: description || '', code: sql,
      spec: JSON.stringify(stored), forked_from: forked_from || null, author: author || 'forge' };
    if (!doc.forked_from) delete doc.forked_from;
    await this.db.write([{ op: 'insert', table: 'sa_capabilities', doc }]);
    const raw = { ...doc, spec: stored };
    this.capCache.set(id, raw);
    // As it runs: the code it was given (its SQL gives back the same).
    const row = { ...this.runs(raw), code: code || '' };
    this.made.set(id, { gen: patternsGen(), row });
    // A shared piece's node follows its newest version; an app's own
    // capabilities join the graph when a version of the app goes live.
    if (SHARED_KINDS.includes(kind) || kind === 'pattern') await this.writeGraph([capDoc(row, { lib: LIB })]);
    return id;
  }

  // What is live: each app's capabilities in its current version, and the
  // newest version of every shared component. Older versions stay in Strata
  // (going back needs them) but the library offers only what is live.
  async liveMap() {
    const starters = async (map) => {
      for (const c of (await this.starters()).values()) map.set(`${LIB}/${c.kind}/${c.slug}`, c.id);
      for (const c of (await this.patterns()).values()) map.set(`${LIB}/pattern/${c.slug}`, c.id);
      return map;
    };
    try {
      const rows = await this.db.query(`SELECT id, live FROM sa_graph WHERE kind != 'app' LIMIT 10000`);
      return starters(new Map(rows.filter((r) => r.live).map((r) => [r.id, r.live])));
    } catch (e) {
      this.log(`graph: ${e.message}; reading what is live from every app`);
    }
    const live = new Map(); // app/kind/slug -> id
    for (const a of await this.listApps()) {
      if (!Number(a.head)) continue;
      const view = await this.resolve(a.id).catch(() => null);
      for (const c of view?.caps || []) if (!SHARED_KINDS.includes(c.kind)) live.set(`${c.app}/${c.kind}/${c.slug}`, c.id);
    }
    for (const c of (await this.shared()).values()) live.set(`${LIB}/${c.kind}/${c.slug}`, c.id);
    return starters(live);
  }

  // Pieces that fit a text, two rankings fused (reciprocal rank fusion):
  // BM25 over what pieces are and do and over their code, and the cosine of
  // the text's embedding and theirs, so "let people pay for their basket"
  // finds a checkout with none of its words. A piece found by meaning alone
  // must be close (MEANING_MIN). A hit on any version of a piece counts for
  // its live version. Each result says how it was found: words, meaning or
  // both, and its cosine.
  async searchCapabilities(text, { excludeApp, kinds, limit = 6 } = {}) {
    const raw = String(text || '').trim();
    const words = raw.replace(/[^\p{L}\p{N}\s]/gu, ' ').trim();
    if (!words) return [];
    const where = [];
    const params = { t: words };
    if (excludeApp) { where.push('app != :x'); params.x = excludeApp; }
    if (kinds?.length) { where.push(`kind IN (${kinds.map((k, i) => `:k${i}`).join(', ')})`); kinds.forEach((k, i) => { params[`k${i}`] = k; }); }
    const filter = where.length ? 'WHERE ' + where.join(' AND ') : '';
    const byWords = this.db.query(`SELECT id, slug, kind, app, score() AS score FROM sa_capabilities ${filter}
      ORDER BY bm25(title, :t) * 2 + bm25(description, :t) + 0.3 * bm25(code, :t) DESC LIMIT 200`, params)
      .catch((e) => { this.log(`search: ${e.message}`); return []; });
    const byMeaning = this.meaning
      ? this.db.query(`SELECT id, slug, kind, app, score() AS score FROM sa_capabilities ${filter}
        ORDER BY similarity(meaning, :q) DESC LIMIT 100`, { ...params, q: { embed: raw.slice(0, 2000), field: 'meaning', table: 'sa_capabilities' } })
        .catch((e) => { this.log(`search by meaning: ${e.message}`); return []; })
      : [];
    const [wordRows, meaningRows] = await Promise.all([byWords, byMeaning]);
    const live = await this.liveMap();
    // Each ranking, by live piece, best first, a piece once.
    const ranking = (rows, keep) => {
      const out = new Map();
      for (const r of rows) {
        if (!keep(Number(r.score))) continue;
        const id = live.get(`${r.app}/${r.kind}/${r.slug}`);
        if (id && !out.has(id)) out.set(id, Number(r.score));
      }
      return out;
    };
    const w = ranking(wordRows, (x) => x > 0);
    const m = ranking(meaningRows, (x) => x >= MEANING_MIN);
    const fused = new Map();
    for (const r of [w, m]) [...r.keys()].forEach((id, i) => fused.set(id, (fused.get(id) || 0) + 1 / (60 + i)));
    const ids = [...fused.entries()].sort((a, b) => b[1] - a[1]).slice(0, limit).map(([id]) => id);
    return (await this.getCapabilities(ids)).map((c) => {
      const { code, ...rest } = c;
      const found = w.has(c.id) && m.has(c.id) ? 'both' : w.has(c.id) ? 'words' : 'meaning';
      return { ...rest, version: Number(c.version), score: fused.get(c.id), found, ...(m.has(c.id) ? { cosine: Math.round(m.get(c.id) * 1000) / 1000 } : {}) };
    });
  }

  async liveCapabilities() {
    const ids = [...(await this.liveMap()).values()];
    return (await this.getCapabilities(ids)).map(({ code, ...rest }) => ({ ...rest, version: Number(rest.version) }))
      .sort((a, b) => String(b.created).localeCompare(String(a.created)));
  }

  // ---- compositions: an app's versions

  async compositions(app) {
    return this.db.query('SELECT id, n, caps, request, summary, created FROM sa_compositions WHERE app = :a ORDER BY n DESC LIMIT 100', { a: app });
  }

  async composition(app, n) {
    if (!n) return { n: 0, caps: [] };
    const [row] = await this.db.get('sa_compositions', [`${app}#${n}`]);
    return row ? { ...row, caps: row.caps || [] } : { n: 0, caps: [] };
  }

  // A new version of the app; it goes live unless {head: false} (a version
  // to try beside the live one).
  async writeComposition(app, caps, request, summary, { head = true } = {}) {
    const rows = await this.db.query('SELECT n FROM sa_compositions WHERE app = :a ORDER BY n DESC LIMIT 1', { a: app });
    const n = rows.length ? Number(rows[0].n) + 1 : 1;
    await this.db.write([
      { op: 'insert', table: 'sa_compositions', doc: { id: `${app}#${n}`, app, n, caps, request: request || '', summary: summary || '' } },
    ]);
    if (head) await this.updateApp(app, { head: n });
    return n;
  }

  async setHead(app, n) {
    await this.composition(app, n); // exists?
    await this.updateApp(app, { head: n });
  }

  // The app as it runs now: its head composition, with every capability. With
  // n, another version of it (one to try, or an older one).
  async resolve(appId, n = null) {
    const app = await this.getApp(appId);
    if (!app) throw new Error(`no app ${appId}`);
    if (n && Number(n) !== Number(app.head)) return this.resolveAt(app, Number(n));
    const cached = this.resolved.get(appId);
    if (cached && cached.head === app.head && cached.gen === patternsGen()) return cached.view;
    const comp = await this.composition(appId, app.head);
    const gen = patternsGen();
    const view = viewOf(app, comp, await this.getCapabilities(comp.caps));
    this.resolved.set(appId, { head: app.head, gen, view });
    return view;
  }

  async resolveAt(app, n) {
    const key = `${app.id}#${n}@${patternsGen()}`;
    let view = this.resolvedAt.get(key);
    if (!view) {
      const comp = await this.composition(app.id, n);
      if (!comp.n) throw Object.assign(new Error(`no version ${n} of this app`), { status: 404 });
      view = viewOf(app, comp, await this.getCapabilities(comp.caps));
      if (this.resolvedAt.size > 200) this.resolvedAt.clear();
      this.resolvedAt.set(key, view);
    }
    return { ...view, app };
  }

  forget(appId) { this.resolved.delete(appId); }

  // ---- shared components

  // The newest version of every shared component and helper, by kind/name.
  async shared() {
    const rows = await this.db.query(`SELECT id, slug, version, kind, title, description, spec, author, created FROM sa_capabilities
      WHERE kind IN ('component', 'helper') ORDER BY version DESC LIMIT 5000`);
    const out = new Map();
    for (const r of rows) {
      const key = `${r.kind}/${r.slug}`;
      if (!out.has(key)) out.set(key, { ...r, version: Number(r.version), spec: parseSpec(r.spec) });
    }
    return out;
  }

  // The built-in starters (views and functions an app copies as they are),
  // the newest version of each, by kind/slug.
  async starters() {
    const rows = await this.db.query(`SELECT id, slug, version, kind, title, description, spec, author, created FROM sa_capabilities
      WHERE app = :lib AND kind IN ('view', 'function') ORDER BY version DESC LIMIT 2000`, { lib: LIB });
    const out = new Map();
    for (const r of rows) {
      const key = `${r.kind}/${r.slug}`;
      if (!out.has(key)) out.set(key, { ...r, version: Number(r.version), spec: parseSpec(r.spec) });
    }
    return out;
  }

  // The screen patterns, built in and added by apps, the newest version of
  // each, by name, with their code (the pattern as JSON).
  async patterns() {
    const rows = await this.db.query(`SELECT id, slug, version, kind, app, title, description, code, spec, author, created FROM sa_capabilities
      WHERE app = :lib AND kind = 'pattern' ORDER BY version DESC LIMIT 2000`, { lib: LIB });
    const out = new Map();
    for (const r of rows) {
      if (out.has(r.slug)) continue;
      const raw = { ...r, version: Number(r.version), spec: parseSpec(r.spec) };
      let code = raw.code;
      try { code = fromSource(raw).code; } catch { /* as stored */ }
      out.set(r.slug, { ...raw, code, sql: r.code });
    }
    return out;
  }

  // Which apps use which shared piece, and at which version (each app pins
  // one), by kind/name.
  async sharedUsers() {
    const users = new Map(); // kind/slug -> [{app, name, id, version}]
    for (const a of await this.listApps()) {
      if (!Number(a.head)) continue;
      const view = await this.resolve(a.id).catch(() => null);
      for (const c of view?.shared || []) {
        const key = `${c.kind}/${c.slug}`;
        if (!users.has(key)) users.set(key, []);
        users.get(key).push({ app: a.id, name: a.name, id: c.id, version: Number(c.version) });
      }
    }
    return users;
  }

  // ---- the graph of apps and pieces (graph.mjs)

  async writeGraph(docs, deletes = []) {
    const ops = [...docs.map((doc) => ({ op: 'upsert', table: 'sa_graph', doc })), ...deletes.map((key) => ({ op: 'delete', table: 'sa_graph', key }))];
    if (!ops.length) return;
    try { await this.db.write(ops); } catch (e) { this.log(`graph: ${e.message}`); }
    this.walkers?.delete('sa_graph');
  }

  // A graph query, run by Strata; or, on a cluster, whose coordinator
  // refuses graph queries, answered by `here(walker)` from the table's
  // nodes, which the engine reads and walks itself (walk.mjs).
  async walk(sql, params, table, cols, here) {
    if (!this.walkHere) {
      try { return await this.db.query(sql, params); } catch (e) {
        if (!isWalkRefused(e)) throw e;
        this.walkHere = true;
        this.log('graph: this ChloeDB is a coordinator over shards, which walks no graphs; the engine walks them itself');
      }
    }
    return here(await this.walker(table, cols));
  }

  // A table's nodes, read again after a write of the graph, or 5 s on.
  async walker(table, cols) {
    this.walkers ||= new Map();
    const had = this.walkers.get(table);
    if (had && Date.now() - had.at < 5000) return had.walker;
    const rows = await this.db.query(`SELECT ${cols} FROM ${table} LIMIT 10000`);
    const walker = new Walker(rows);
    this.walkers.set(table, { at: Date.now(), walker });
    return walker;
  }

  // An app's nodes follow the version that is live: written again whenever
  // the app changes, and its nodes that the new version lacks are removed.
  async syncAppGraph(appId) {
    let view;
    try { view = await this.resolve(appId); } catch { return; }
    const docs = appDocs(view, SHARED_KINDS);
    const keep = new Set(docs.map((d) => d.id));
    let old = [];
    try { old = await this.db.query('SELECT id FROM sa_graph WHERE app = :a LIMIT 10000', { a: appId }); } catch { /* none */ }
    await this.writeGraph(docs, old.map((r) => r.id).filter((id) => !keep.has(id)));
  }

  // The whole graph from what Strata holds, at start-up.
  async rebuildGraph() {
    const docs = [];
    const shared = [...(await this.shared()).values()];
    for (const c of await this.getCapabilities(shared.map((x) => x.id))) docs.push(capDoc(c, { lib: LIB }));
    for (const c of (await this.patterns()).values()) docs.push(capDoc(c, { lib: LIB }));
    for (const a of await this.listApps()) {
      const view = await this.resolve(a.id).catch(() => null);
      if (view) docs.push(...appDocs(view, SHARED_KINDS));
    }
    const keep = new Set(docs.map((d) => d.id));
    let old = [];
    try { old = await this.db.query('SELECT id FROM sa_graph LIMIT 10000'); } catch { /* none */ }
    await this.writeGraph(docs, old.map((r) => r.id).filter((id) => !keep.has(id)));
    this.log(`graph: ${docs.length} nodes, ${docs.reduce((n, d) => n + d.links.length, 0)} edges`);
  }

  // Nodes with their edges: every one, or an app's and what it reaches.
  async graph({ app } = {}) {
    const cols = 'id, kind, app, slug, title, description, live, version, links';
    const rows = app
      ? await this.walk(`SELECT ${cols}, hops() AS hops FROM sa_graph WHERE REACHABLE(links, :n, hops => 6, min_hops => 0) ORDER BY hops() LIMIT 2000`, { n: appNode(app) },
        'sa_graph', GRAPH_COLS, (w) => w.rowsByHops(w.reachable([appNode(app)], { hops: 6, minHops: 0 })))
      : await this.db.query(`SELECT ${cols} FROM sa_graph LIMIT 5000`);
    return rows.map((r) => ({ ...r, version: Number(r.version) || 0, links: parseLinks(r.links) }));
  }

  // What depends on a node, directly or through others: the views,
  // functions and pieces that use it, and the apps they belong to, nearest
  // first. A view that only opens another does not depend on its pieces.
  async dependents(id, { hops = 8 } = {}) {
    return this.walk(`SELECT id, kind, app, slug, title, hops() AS hops FROM sa_graph
      WHERE REACHABLE(links, :n, hops => ${Number(hops)}, dir => 'in', label => 'uses,has,calls') ORDER BY hops(), id LIMIT 2000`, { n: id },
      'sa_graph', GRAPH_COLS, (w) => w.rowsByHops(w.reachable([id], { hops: Number(hops), dir: 'in', label: 'uses,has,calls' })).map(nodeRow));
  }

  // The nodes a walk from some nodes reaches along edges with these labels
  // (dir 'in': what points at them; 'out': what they point at), nearest
  // first, with their edges.
  async reach(seeds, { dir = 'in', label = '', hops = 1 } = {}) {
    const ids = [...new Set(seeds.filter(Boolean))];
    if (!ids.length) return [];
    const labels = String(label).split(',').map((l) => l.trim()).filter((l) => /^[a-z]+$/.test(l)).join(',');
    const h = Math.max(1, Math.min(8, Number(hops) || 1));
    const way = dir === 'out' ? 'out' : 'in';
    const rows = await this.walk(`SELECT id, kind, app, slug, title, live, links, hops() AS hops FROM sa_graph
      WHERE REACHABLE(links, :s, hops => ${h}, dir => '${way}'${labels ? `, label => '${labels}'` : ''}) ORDER BY hops(), id LIMIT 2000`, { s: ids },
      'sa_graph', GRAPH_COLS, (w) => w.rowsByHops(w.reachable(ids, { hops: h, dir: way, label: labels })));
    return rows.map((r) => ({ ...r, hops: Number(r.hops) || 0, links: parseLinks(r.links) }));
  }

  // What a node needs: the pieces, tables and functions it reaches.
  async needs(id, { hops = 8 } = {}) {
    return this.walk(`SELECT id, kind, app, slug, title, hops() AS hops FROM sa_graph
      WHERE REACHABLE(links, :n, hops => ${Number(hops)}, label => 'has,uses,data,calls,on') ORDER BY hops(), id LIMIT 2000`, { n: id },
      'sa_graph', GRAPH_COLS, (w) => w.rowsByHops(w.reachable([id], { hops: Number(hops), label: 'has,uses,data,calls,on' })).map(nodeRow));
  }

  // Shared pieces near some nodes, by personalized PageRank over every
  // edge, both ways: from a piece, the pieces the same views also use and
  // the ones it calls; from an app, the pieces apps like it use.
  async related(seeds, { kinds = SHARED_KINDS, limit = 20, exclude = [] } = {}) {
    const ids = [...new Set(seeds.filter(Boolean))];
    if (!ids.length) return [];
    const skip = new Set([...ids, ...exclude]);
    const params = { s: ids };
    kinds.forEach((k, i) => { params[`k${i}`] = k; });
    let rows;
    try {
      rows = await this.walk(`SELECT id, kind, slug, title, score() AS score FROM sa_graph WHERE kind IN (${kinds.map((k, i) => `:k${i}`).join(', ')})
        ORDER BY ppr(links, :s) DESC LIMIT ${Number(limit) + skip.size}`, params,
        'sa_graph', GRAPH_COLS, (w) => w.byScore(w.ppr(ids), (r) => kinds.includes(r.kind), Number(limit) + skip.size).map(({ id, kind, slug, title, score }) => ({ id, kind, slug, title, score })));
    } catch (e) { this.log(`graph: ${e.message}`); return []; }
    return rows.filter((r) => !skip.has(r.id) && Number(r.score) > 0).slice(0, limit).map((r) => ({ ...r, score: Number(r.score) }));
  }

  // One node: what it uses, what uses it (and which apps), what goes with it.
  async node(id) {
    const [row] = await this.db.get('sa_graph', [id]);
    if (!row) return null;
    const links = parseLinks(row.links);
    const needs = await this.needs(id);
    const dependents = await this.dependents(id);
    const direct = new Set([...links.map((l) => l.to), ...dependents.filter((d) => Number(d.hops) === 1).map((d) => d.id)]);
    const related = await this.related([id], { exclude: [...direct], limit: 8 });
    return { ...row, version: Number(row.version) || 0, links, needs, dependents, related };
  }

  // ---- each app's data tables

  async ensureTable(appId, spec, existing) {
    const name = physical(appId, spec.name);
    const cols = spec.columns || [];
    if (!existing.has(name)) {
      const defs = ['id KEYWORD PRIMARY KEY'];
      // A links table: each row is a link from the row src to the row dst of
      // another table, which graph queries walk.
      if (spec.links) defs.push(`src KEYWORD NOT NULL REFERENCES ${physical(appId, spec.links)}`, `dst KEYWORD NOT NULL REFERENCES ${physical(appId, spec.links)}`);
      for (const c of cols) if (!(spec.links && (c.name === 'src' || c.name === 'dst'))) defs.push(colSql(c));
      defs.push('created TIMESTAMP DEFAULT CURRENT_TIMESTAMP');
      await this.db.sql(`CREATE TABLE ${name} (${defs.join(', ')})${spec.links ? " OPTIONS(edges = 'src, dst')" : ''}`);
      existing.add(name);
      return { created: name, added: [] };
    }
    const tables = await this.db.tables();
    const t = tables.find((x) => tableName(x) === name);
    const have = new Set((t?.schema?.fields || []).map((f) => f.name));
    const added = [];
    for (const c of cols) {
      if (!have.has(c.name)) {
        await this.db.sql(`ALTER TABLE ${name} ADD COLUMN ${colSql(c)}`);
        added.push(c.name);
      }
    }
    return { created: null, added };
  }

  async countRows(table) {
    try {
      const rows = await this.db.query(`SELECT COUNT(*) AS n FROM ${table}`);
      return Number(rows[0]?.n || 0);
    } catch { return 0; }
  }

  // ---- requests, events, suggestions

  async putRequest(doc) {
    await this.db.write([{ op: 'upsert', table: 'sa_requests', doc }]);
  }

  async putTrace(id, app, trace) {
    await this.db.write([{ op: 'upsert', table: 'sa_traces', doc: { id, app, trace: JSON.stringify(trace) } }]);
  }

  async trace(id) {
    const [row] = await this.db.get('sa_traces', [id]);
    if (!row) return null;
    try { return JSON.parse(row.trace); } catch { return null; }
  }

  // Every timeline of an app, for its build statistics.
  async traces(app, limit = 100) {
    const rows = await this.db.query(`SELECT id, trace FROM sa_traces WHERE app = :a LIMIT ${limit}`, { a: app });
    return rows.map((r) => { try { return { id: r.id, ...JSON.parse(r.trace) }; } catch { return null; } }).filter(Boolean);
  }

  async requests(app, limit = 50) {
    return this.db.query(`SELECT id, text, source, status, comp, summary, error, ms, cost, created FROM sa_requests WHERE app = :a ORDER BY created DESC LIMIT ${limit}`, { a: app });
  }

  // label: the name of the row written ("Monstera"), kept with the event so
  // that a removed row is still told by its name. taps: on a write or a run,
  // the taps in the app since the one before it (engine/measures.mjs).
  async track(app, kind, cap, detail, { person = null, v = null, session = null, screen = null, tbl = null, rid = null, fields = null, q = null, results = null, ms = null, label = null, taps = null } = {}) {
    const doc = { id: newId(12), app, kind, cap: cap || '', detail: detail ? String(detail).slice(0, 500) : '' };
    const word = (x, n) => { const t = String(x ?? '').trim(); return t ? t.slice(0, n) : null; };
    const num = (x) => (x === null || x === undefined || x === '' || !Number.isFinite(Number(x)) ? null : Math.max(0, Math.round(Number(x))));
    const more = {
      person: word(person, 40), v: Number(v) || null, session: word(session, 40), screen: word(screen, 80), tbl: word(tbl, 80), rid: word(rid, 80),
      // The fields changed, sorted: the same change on row after row is the same value.
      fields: word(Array.isArray(fields) ? [...fields].sort().join(',') : fields, 200),
      q: word(q === null || q === undefined ? null : String(q).toLowerCase().replace(/\s+/g, ' '), 120), results: num(results), ms: num(ms), label: word(label, 120), taps: num(taps),
    };
    for (const [k, x] of Object.entries(more)) if (x !== null) doc[k] = x;
    await this.db.write([{ op: 'insert', table: 'sa_events', doc }]);
  }

  // The events since a time (ISO; all of them without one), newest first.
  async events(app, { since = null, limit = 5000 } = {}) {
    const n = Math.max(1, Math.min(10000, Math.floor(limit)));
    return this.db.query(`SELECT kind, cap, detail, person, v, session, screen, tbl, rid, fields, q, results, ms, label, taps, ts FROM sa_events WHERE app = :a${since ? ' AND ts >= :since' : ''} ORDER BY ts DESC LIMIT ${n}`, since ? { a: app, since } : { a: app });
  }

  async recentEvents(app, limit = 200) {
    return this.db.query(`SELECT kind, cap, detail, person, v, session, screen, tbl, rid, fields, q, results, ms, taps, ts FROM sa_events WHERE app = :a ORDER BY ts DESC LIMIT ${limit}`, { a: app });
  }

  async putSuggestions(app, items) {
    if (!items.length) return [];
    const docs = items.map((s) => ({ id: newId(10), app, status: 'open', ...s }));
    for (const d of docs) for (const k of Object.keys(d)) if (d[k] === null || d[k] === undefined) delete d[k];
    await this.db.write(docs.map((doc) => ({ op: 'insert', table: 'sa_suggestions', doc })));
    return docs;
  }

  async suggestions(app) {
    return this.db.query(`SELECT id, text, why, source, cap, status, signal, created FROM sa_suggestions WHERE app = :a AND status = 'open' ORDER BY created DESC LIMIT 12`, { a: app });
  }

  // Every suggestion made to an app, with what became of it.
  async suggestionHistory(app, limit = 40) {
    return this.db.query(`SELECT text, source, status, signal, created FROM sa_suggestions WHERE app = :a ORDER BY created DESC LIMIT ${limit}`, { a: app });
  }

  // ---- candidates: versions the engine built from use, beside the live one

  async putCandidate(doc) {
    const d = { ...doc };
    for (const k of Object.keys(d)) if (d[k] === null || d[k] === undefined) delete d[k];
    await this.db.write([{ op: 'upsert', table: 'sa_candidates', doc: d }]);
    return d;
  }

  async candidate(id) {
    const [row] = await this.db.get('sa_candidates', [id]);
    return row || null;
  }

  async candidates(app, limit = 20) {
    return this.db.query(`SELECT * FROM sa_candidates WHERE app = :a ORDER BY created DESC LIMIT ${limit}`, { a: app });
  }

  async setting(app, name) {
    const [row] = await this.db.get('sa_settings', [`${app}:${name}`]);
    return row ? row.value : null;
  }

  async putSetting(app, name, value) {
    await this.db.write([{ op: 'upsert', table: 'sa_settings', doc: { id: `${app}:${name}`, app, name, value: String(value) } }]);
  }

  async setSuggestion(id, status) {
    const [row] = await this.db.get('sa_suggestions', [id]);
    if (row) await this.db.write([{ op: 'upsert', table: 'sa_suggestions', doc: { ...row, status } }]);
    return row;
  }
}
