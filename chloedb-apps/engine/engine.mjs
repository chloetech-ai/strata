// The runtime: what views and functions can do to an app's data, the
// triggers that run functions after writes, what people did (events), and
// the announcements that keep every open window live.

import { EventEmitter } from 'node:events';
import { AsyncLocalStorage } from 'node:async_hooks';
import { Store, physical, newId } from './store.mjs';
import { rewriteSql, checkReads, tokenize } from './sql.mjs';
import { decide, rulesOf, scopeSql, scopeParams, inScope, fillScope, SCOPE_PARAM } from './access.mjs';
import { runFunction } from './sandbox.mjs';
import { runRule } from './rules.mjs';
import { rowLabel, tablesOf } from './activity.mjs';

const MAX_ROWS = 1000;
// What a person is called where an app shows them: their name, else their
// email, else "you" (never "undefined").
export const personName = (p) => (p && String(p.name || '').trim()) || (p && String(p.email || '').trim()) || 'you';
const MAX_DEPTH = 3;
// What a visitor of a published app may write (checkVisitorWrite): a field
// of at most 10 KB, a row of at most 64 KB, a list of at most 100 items.
export const VISITOR_LIMITS = {
  field: Number(process.env.STRATA_APPS_PUBLIC_FIELD_MAX) || 10 * 1024,
  row: Number(process.env.STRATA_APPS_PUBLIC_ROW_MAX) || 64 * 1024,
  items: 100,
};
const bad = (message) => Object.assign(new Error(message), { status: 400 });
const kb = (n) => (n < 1024 ? `${n} bytes` : `${Math.round(n / 1024)} KB`);

// The values each column of a table may take when its screens offer only a
// fixed list: the options of the select fields of every form on the table,
// with the values the app's own code writes to that column (a row action's
// {status: 'done'}). A column any form takes as free text, or whose options
// come from data, has no list. Map column -> Set of values.
const choiceCache = new WeakMap();
export function choicesOf(view, table) {
  let byTable = choiceCache.get(view);
  if (!byTable) { byTable = new Map(); choiceCache.set(view, byTable); }
  if (byTable.has(table)) return byTable.get(table);
  const lists = new Map();
  const free = new Set();
  const walk = (v) => {
    if (Array.isArray(v)) { v.forEach(walk); return; }
    if (!v || typeof v !== 'object') return;
    if (v.form === table && Array.isArray(v.fields)) {
      for (const f of v.fields) {
        const field = typeof f === 'string' ? { name: f } : f;
        if (!field || !field.name) continue;
        const opts = field.options;
        if ((field.type === 'select' || field.type === 'radio' || field.type === 'segmented') && Array.isArray(opts) && opts.length) {
          const set = lists.get(field.name) || new Set();
          for (const o of opts) set.add(String(o && typeof o === 'object' ? o.value : o));
          lists.set(field.name, set);
        } else free.add(field.name);
      }
    }
    for (const x of Object.values(v)) walk(x);
  };
  for (const c of view.views || []) {
    if (!c.spec?.screen) continue;
    try { walk(JSON.parse(c.code)); } catch { /* a screen that is not JSON offers no list */ }
  }
  for (const k of free) lists.delete(k);
  // What the app's own code writes to a listed column counts as a choice.
  const code = [...(view.views || []), ...(view.functions || [])].map((c) => String(c.code || '')).join('\n');
  for (const [col, set] of lists) {
    const re = new RegExp(`(?:^|[^A-Za-z0-9_])\\\\?["']?${col}\\\\?["']?\\s*[:=]\\s*\\\\?["']([^"'\\\\\\n]{1,80})\\\\?["']`, 'g');
    for (const m of code.matchAll(re)) set.add(m[1]);
  }
  byTable.set(table, lists);
  return lists;
}

// A visitor's write (a published app's public, not its owner): every value
// within the size limits, and a column whose screens offer a fixed list
// takes only a value of it. Throws a 400 that says what is wrong.
export function checkVisitorWrite(view, spec, fields, limits = VISITOR_LIMITS) {
  const size = (x) => Buffer.byteLength(typeof x === 'string' ? x : JSON.stringify(x ?? null));
  const whole = size(fields || {});
  if (whole > limits.row) throw bad(`a row is at most ${kb(limits.row)} (this one is ${kb(whole)})`);
  if (fields?.id !== undefined && (typeof fields.id !== 'string' || fields.id.length > 80)) throw bad('id is text of at most 80 characters');
  const choices = choicesOf(view, spec.name);
  for (const [k, v] of Object.entries(fields || {})) {
    if (v === null || v === undefined || v === '') continue;
    if (Array.isArray(v) && v.length > limits.items) throw bad(`${k} takes at most ${limits.items} items`);
    const n = size(v);
    if (n > limits.field) throw bad(`${k} is at most ${kb(limits.field)} (this is ${kb(n)})`);
    const allowed = choices.get(k);
    if (allowed && ![].concat(v).every((x) => allowed.has(String(x)))) {
      throw bad(`${k} must be one of ${[...allowed].join(', ')}`);
    }
  }
}

// The person a request is for, as SQL parameters: :user_id, :user_email,
// :user_name and :company_id are the engine's to bind. For an app's people
// (ctx.as) they are always the signed-in person's and the company they work
// in now, whatever the call sent, so a page cannot ask as someone else; for
// the studio, a value the call gives stays. A query that names one with no
// value is refused as a 401 (sign in first), not sent to ChloeDB unbound.
const PEOPLE_PARAMS = ['user_id', 'user_email', 'user_name', 'company_id'];
function bindPeople(sql, params, ctx = {}) {
  const p = { ...(params || {}) };
  delete p[SCOPE_PARAM.user];
  delete p[SCOPE_PARAM.company];
  let named;
  try { named = new Set(tokenize(String(sql || '')).filter((t) => t.k === 'param').map((t) => t.v.slice(1))); } catch { return p; }
  if (!PEOPLE_PARAMS.some((k) => named.has(k))) return p;
  const me = ctx.as ? ctx.as.user : ctx.user || null;
  const company = ctx.as ? ctx.as.company : ctx.company || null;
  const ours = { user_id: me?.id, user_email: me?.email, user_name: me?.name, company_id: company };
  for (const k of PEOPLE_PARAMS) {
    if (!named.has(k)) continue;
    if (ctx.as || p[k] === undefined || p[k] === null) { if (ours[k] != null) p[k] = ours[k]; else delete p[k]; }
    if (p[k] === undefined || p[k] === null) {
      throw Object.assign(new Error(k === 'company_id' ? 'Join or choose a company first: this needs the company you work in' : 'Sign in first: this needs someone signed in'), { status: 401, refused: 'sign-in' });
    }
  }
  return p;
}

function coerce(type, v, col) {
  if (v === null || v === undefined || v === '') return null;
  switch (type) {
    case 'int': {
      const n = typeof v === 'boolean' ? Number(v) : Number(v);
      if (!Number.isFinite(n)) throw new Error(`${col} takes a whole number, not ${JSON.stringify(v)}`);
      return Math.round(n);
    }
    case 'float': {
      const n = Number(v);
      if (!Number.isFinite(n)) throw new Error(`${col} takes a number, not ${JSON.stringify(v)}`);
      return n;
    }
    case 'bool':
      if (typeof v === 'boolean') return v;
      if (v === 1 || v === '1' || v === 'true' || v === 'yes' || v === 'on') return true;
      if (v === 0 || v === '0' || v === 'false' || v === 'no' || v === 'off') return false;
      throw new Error(`${col} takes true or false, not ${JSON.stringify(v)}`);
    case 'timestamp': {
      const d = typeof v === 'number' ? new Date(v) : new Date(String(v));
      if (Number.isNaN(d.getTime())) throw new Error(`${col} takes a time (ISO 8601), not ${JSON.stringify(v)}`);
      return d.toISOString();
    }
    case 'keyword[]': {
      const list = Array.isArray(v) ? v : String(v).split(',');
      return list.map((x) => String(x).trim()).filter(Boolean);
    }
    case 'keyword':
      return String(v).slice(0, 1000);
    default:
      return typeof v === 'string' ? v : (typeof v === 'object' ? JSON.stringify(v) : String(v));
  }
}

// Whether an error a browser reported says only that a call was not
// answered: the browser's own words for a failed fetch, the command center's
// ("no answer: the network failed..."), a proxy's 502 to 504, an engine or a
// server not answering. Not a fault of the app's code, so never a repair.
const UNANSWERED = /\b(no answer\b|failed to fetch|networkerror|network error|load failed|the network failed|does not answer|is not answering|not answering|bad gateway|service unavailable|gateway time-?out|econnreset|econnrefused|socket hang up|the answer was cut off|too many requests)/i;
export function unanswered(message) {
  return UNANSWERED.test(String(message || '').split('\n')[0]);
}

export class Engine extends EventEmitter {
  constructor(strata, { log = console.log } = {}) {
    super();
    this.setMaxListeners(1000);
    this.db = strata;
    this.store = new Store(strata, log);
    this.log = log;
    this.feed = false;
    this.pending = new Set();
    this.locks = new Map(); // app -> the end of its queue of rules that write
    // The version a request runs on, when it is not the live one (someone
    // trying a version the engine built): everything the request starts,
    // triggers included, runs on that version.
    this.at = new AsyncLocalStorage();
    this.services = null; // set by the server: people, files, email, places
    this.reach = null; // set by the server: a model and the web for apps (reach.mjs)
    this.keys = null; // set by the server: each app's own key (appkeys.mjs)
  }

  // The client app `appId`'s code reads and writes with: its own key's,
  // which the server holds to the app's tables, or the engine's.
  dbOf(appId) {
    return this.keys ? this.keys.client(appId) : this.db;
  }

  async init() {
    await this.store.init();
    // Strata's feed of committed transactions is how every window learns of
    // a change, whoever made it: this engine, another engine on the same
    // database, or a client writing to Strata directly.
    this.feed = await this.db.follow((txn) => this.onTxn(txn), this.log);
    this.log(this.feed ? 'following ChloeDB\'s feed of changes' : 'ChloeDB serves no feed here: the engine announces its own writes');
  }

  onTxn(txn) {
    const data = new Map();
    for (const op of txn.ops || []) {
      const t = op.table || '';
      const m = t.match(/^app_([a-z0-9]{6})_(.+)$/);
      if (m) {
        if (!data.has(m[1])) data.set(m[1], new Set());
        data.get(m[1]).add(m[2]);
      } else if (t === 'sa_apps' && op.doc?.id) {
        this.store.forget(op.doc.id);
        this.emit('app', { app: op.doc.id, head: op.doc.head });
      } else if (t === 'sa_suggestions' && op.doc?.app) {
        this.emit('suggestions', { app: op.doc.app });
      } else if (t === 'sa_requests' && op.doc?.app) {
        this.emit('request', { app: op.doc.app, id: op.doc.id, status: op.doc.status });
      }
    }
    for (const [app, tables] of data) this.emit('data', { app, tables: [...tables] });
  }

  // Without a feed, writes made here announce themselves.
  announce(kind, payload) {
    if (!this.feed) this.emit(kind, payload);
  }

  // Runs fn on version v of the app (the live one when v is empty).
  atVersion(appId, v, fn) {
    return v ? this.at.run({ app: appId, v: Number(v) }, fn) : fn();
  }

  // The app as the running request sees it.
  view(appId) {
    const s = this.at.getStore();
    return this.store.resolve(appId, s && s.app === appId ? s.v : null);
  }

  async tableSpec(appId, table) {
    const view = await this.view(appId);
    const cap = view.tables.find((t) => t.spec.name === table);
    if (!cap) {
      const names = view.tables.map((t) => t.spec.name).join(', ') || 'none yet';
      throw new Error(`no table ${table} in this app (its tables: ${names})`);
    }
    return cap.spec;
  }

  makeDoc(spec, fields, { partial = false } = {}) {
    const types = new Map((spec.columns || []).map((c) => [c.name, c.type]));
    const doc = {};
    for (const [k, v] of Object.entries(fields || {})) {
      if (k === 'id' || k === 'created') continue;
      if (!types.has(k)) {
        throw new Error(`table ${spec.name} has no column ${k} (its columns: id, created, ${[...types.keys()].join(', ')})`);
      }
      doc[k] = coerce(types.get(k), v, `${spec.name}.${k}`);
    }
    if (!partial) for (const k of types.keys()) if (!(k in doc)) doc[k] = null;
    return doc;
  }

  // ---- what views and functions call

  // ctx.as is the person a read is for, when it is one of the app's people
  // (a visitor or a member, through the public path): the tables' declared
  // access (access.mjs) holds them to what they may see. Without it (the
  // studio, the engine's own checks, triggers) every row is read.
  async sql(appId, sql, params, ctx = {}) {
    const view = await this.view(appId);
    const names = view.tables.map((t) => t.spec.name);
    const who = ctx.as || null;
    const specs = new Map(view.tables.map((t) => [t.spec.name, t.spec]));
    const scope = who ? (name) => {
      const d = decide(specs.get(name), 'read', who);
      return d.all ? null : { sql: (alias, o) => scopeSql(d.scope, alias, o) };
    } : null;
    const { sql: phys } = rewriteSql(sql, names, (t) => physical(appId, t), { scope });
    params = { ...bindPeople(sql, params, ctx), ...(who ? scopeParams(who) : {}) };
    const db = await this.dbOf(appId);
    // With the app's own key the server refuses any other table itself;
    // with the engine's, its plan is checked first.
    if (!db.own) await checkReads(db, phys, params, names.map((t) => physical(appId, t)));
    let rows = await db.query(phys, params || {});
    if (rows.length > MAX_ROWS) rows = rows.slice(0, MAX_ROWS);
    // Strata names a qualified column as written (`t.title`); code expects `title`.
    return rows.map((r) => {
      let out = r;
      for (const k of Object.keys(r)) {
        const i = k.lastIndexOf('.');
        if (i > 0) {
          const short = k.slice(i + 1);
          if (!(short in r)) {
            if (out === r) out = { ...r };
            out[short] = r[k];
            delete out[k];
          }
        }
      }
      return out;
    });
  }

  async insert(appId, table, fields, ctx = {}) {
    const spec = await this.tableSpec(appId, table);
    fields = this.mayWrite(spec, 'add', fields, ctx);
    if (ctx.visitor) checkVisitorWrite(await this.view(appId), spec, fields);
    const doc = this.makeDoc(spec, fields);
    for (const k of Object.keys(doc)) if (doc[k] === null) delete doc[k];
    doc.id = fields?.id ? String(fields.id) : newId(10);
    if (fields?.created) doc.created = coerce('timestamp', fields.created, 'created');
    const db = await this.dbOf(appId);
    await db.write([{ op: 'insert', table: physical(appId, table), doc }]);
    const [row] = await db.get(physical(appId, table), [doc.id]);
    this.announce('data', { app: appId, tables: [table] });
    await this.after(appId, table, 'insert', row || doc, ctx);
    return row || doc;
  }

  async update(appId, table, id, fields, ctx = {}) {
    const spec = await this.tableSpec(appId, table);
    const db = await this.dbOf(appId);
    const [old] = await db.get(physical(appId, table), [String(id)]);
    if (!old) throw new Error(`no row ${id} in ${table}`);
    if (ctx.visitor) checkVisitorWrite(await this.view(appId), spec, fields);
    this.mayChange(spec, 'change', old, ctx);
    if (ctx.ownRows && !rulesOf(spec)) await this.ownedBy(appId, table, old.id, ctx.who);
    const changes = this.makeDoc(spec, fields, { partial: true });
    const doc = { ...old, ...changes, id: old.id };
    this.mayChange(spec, 'change', doc, ctx, { moved: true });
    for (const k of Object.keys(doc)) if (doc[k] === null) delete doc[k];
    await db.write([{ op: 'upsert', table: physical(appId, table), doc }]);
    this.announce('data', { app: appId, tables: [table] });
    await this.after(appId, table, 'update', doc, { ...ctx, changed: Object.keys(changes) });
    return doc;
  }

  async remove(appId, table, id, ctx = {}) {
    const spec = await this.tableSpec(appId, table);
    const db = await this.dbOf(appId);
    const [old] = await db.get(physical(appId, table), [String(id)]);
    this.mayChange(spec, 'remove', old, ctx);
    if (ctx.ownRows && !rulesOf(spec)) await this.ownedBy(appId, table, String(id), ctx.who);
    await db.write([{ op: 'delete', table: physical(appId, table), key: String(id) }]);
    this.announce('data', { app: appId, tables: [table] });
    if (old) await this.after(appId, table, 'delete', old, ctx);
    // The row as it was, in a form app.insert takes back: the frame's Undo.
    const row = old ? Object.fromEntries(Object.entries(old).filter(([k]) => k === 'id' || k === 'created' || (spec.columns || []).some((c) => c.name === k))) : null;
    return { removed: old ? 1 : 0, row };
  }

  // A write by one of the app's people (ctx.as), against the table's
  // declared access: an add under a scope gets the scope's columns filled
  // (and is refused when it names someone else). Answers the fields to write.
  mayWrite(spec, action, fields, ctx) {
    if (!ctx.as) return fields;
    const d = decide(spec, action, ctx.as);
    return d.all ? fields : fillScope(fields, d.scope, ctx.as, spec.name);
  }

  // A change or remove of row `row` by one of the app's people: allowed on
  // the table, and the row in their scope (with moved, the row as it would
  // be stored: a change cannot move a row out of the scope).
  mayChange(spec, action, row, ctx, { moved = false } = {}) {
    if (!ctx.as) return;
    const d = decide(spec, action, ctx.as);
    if (d.all || !row) return;
    if (!inScope(row, d.scope, ctx.as)) {
      throw Object.assign(new Error(moved ? `You cannot give ${spec.name} rows to someone else (${d.scope.map((t) => t.col).join(' or ')})` : `You can ${action} only your own rows of ${spec.name}`), { status: 403, refused: 'access' });
    }
  }

  async call(appId, slug, input, ctx = {}) {
    const view = await this.view(appId);
    const fn = view.functions.find((f) => f.slug === slug);
    if (!fn) {
      const names = view.functions.map((f) => f.slug).join(', ') || 'none';
      throw new Error(`no function ${slug} in this app (its functions: ${names})`);
    }
    const depth = (ctx.depth || 0) + 1;
    if (depth > MAX_DEPTH + 1) throw new Error(`functions called each other more than ${MAX_DEPTH} deep`);
    // The person whose action led here (a view's call, or a write that fired
    // a trigger) stays the user all the way down.
    const user = ctx.user || null;
    // A build's tests (checks.mjs) reach no model and no web: app.ask and
    // app.fetch answer empty there.
    // A visitor's call (a published app's public) stays one all the way
    // down: its writes are checked as theirs, its errors only recorded.
    // A person's call (ctx.as) runs as them: its reads and writes are held
    // to the tables' declared access, all the way down.
    const inner = { depth, by: fn.id, user, test: !!ctx.test, visitor: !!ctx.visitor, pub: !!(ctx.pub || ctx.visitor), ...(ctx.as ? { as: ctx.as } : {}) };
    const services = () => {
      if (!this.services) throw new Error('this engine runs without services (people, files, email, places)');
      return this.services;
    };
    const reach = () => {
      if (!this.reach) throw new Error('this engine runs without a model or the web for apps');
      return this.reach;
    };
    // A rule with steps writes through a batch (its own, or the one of the
    // rule that called it), committed when the outermost rule ends.
    const steps = !!fn.spec.rule && !JSON.parse(fn.code).query;
    const batch = steps ? ctx.batch || this.batch(appId, inner) : null;
    // (A JavaScript function a rule calls passes the rule's batch on, so a
    // rule it calls in turn joins it rather than waiting for it.)
    if (batch || ctx.batch) inner.batch = batch || ctx.batch;
    const host = async (method, args) => {
      switch (method) {
        case 'sql': return this.sql(appId, args[0], args[1], inner);
        case 'insert': return batch ? batch.insert(args[0], args[1]) : this.insert(appId, args[0], args[1], inner);
        case 'update': return batch ? batch.update(args[0], args[1], args[2]) : this.update(appId, args[0], args[1], args[2], inner);
        case 'remove': return batch ? batch.remove(args[0], args[1]) : this.remove(appId, args[0], args[1], inner);
        case 'call': return (await this.call(appId, args[0], args[1], inner)).result;
        case 'people': return services().people(appId);
        // A rule's email goes once its writes are stored, so a rule that stops sends none.
        case 'email': if (batch) { const msg = args[0]; batch.later(() => services().email(appId, msg, fn.id)); return { queued: true }; } return services().email(appId, args[0], fn.id);
        case 'geocode': return services().geocode(args[0]);
        case 'files': return services().fileInfo(appId, args[0]);
        case 'ask': return reach().ask(appId, args[0], args[1] || {}, { test: inner.test });
        case 'fetch': return reach().fetch(appId, args[0], args[1] || {}, { test: inner.test });
        default: throw new Error(`no app.${method}`);
      }
    };
    try {
      // A rule runs here, in the engine; JavaScript runs in the sandbox.
      // A call answers {result, logs, ms}, from a rule as from JavaScript.
      const started = Date.now();
      const rule = async () => ({ result: await runRule(JSON.parse(fn.code), { user: user ? { id: user.id, name: personName(user), email: user.email } : null, ...(input || {}) }, host), logs: [], ms: Date.now() - started });
      let out;
      if (batch && !ctx.batch) out = await this.exclusive(appId, async () => { const r = await rule(); await batch.commit(); return r; });
      else if (fn.spec.rule) out = await rule();
      else out = await runFunction(fn.code, input, host, { user, helpers: view.helpers.map((c) => ({ name: c.slug, code: c.code })) });
      if (!ctx.depth && !ctx.test) this.store.track(appId, 'call', fn.id, slug, { person: ctx.who || user?.id, v: view.comp.n, session: ctx.session, screen: ctx.screen, ms: out.ms, taps: ctx.taps, src: ctx.owner ? 'owner' : null }).catch(() => {});
      return out;
    } catch (e) {
      // A rule's refusal ("Only 2 left") is for the person, not a repair.
      if (!e.refused) this.reportError(appId, fn.id, e.message, ctx.trigger ? `trigger on ${ctx.trigger}` : 'call', { visitor: !!(ctx.visitor || ctx.pub) });
      throw e;
    }
  }

  // Runs fn when the app's earlier rules that write have finished, so two
  // never overlap: what one checks with "require" still holds when it writes.
  async exclusive(appId, fn) {
    const prev = this.locks.get(appId) || Promise.resolve();
    let release;
    const mine = new Promise((r) => { release = r; });
    const tail = prev.then(() => mine);
    this.locks.set(appId, tail);
    await prev;
    try { return await fn(); } finally {
      release();
      if (this.locks.get(appId) === tail) this.locks.delete(appId);
    }
  }

  // A rule's writes, kept until it ends, then written in one request (all or
  // nothing); the triggers on them run after. Reads during the rule see the
  // data as it was (a row this batch changed is read back from the batch).
  batch(appId, ctx) {
    const ops = [];
    const rows = new Map();
    const events = [];
    const afterwards = [];
    const key = (table, id) => `${table}\u0000${id}`;
    const current = async (table, id) => {
      if (rows.has(key(table, id))) return rows.get(key(table, id));
      const [old] = await (await this.dbOf(appId)).get(physical(appId, table), [String(id)]);
      return old || null;
    };
    return {
      insert: async (table, fields) => {
        const spec = await this.tableSpec(appId, table);
        fields = this.mayWrite(spec, 'add', fields, ctx);
        const doc = this.makeDoc(spec, fields);
        for (const k of Object.keys(doc)) if (doc[k] === null) delete doc[k];
        doc.id = fields?.id ? String(fields.id) : newId(10);
        ops.push({ op: 'insert', table: physical(appId, table), doc });
        rows.set(key(table, doc.id), doc);
        events.push([table, 'insert', doc]);
        return doc;
      },
      update: async (table, id, fields) => {
        const spec = await this.tableSpec(appId, table);
        const old = await current(table, String(id));
        if (!old) throw new Error(`no row ${id} in ${table}`);
        this.mayChange(spec, 'change', old, ctx);
        const changes = this.makeDoc(spec, fields, { partial: true });
        const doc = { ...old, ...changes, id: old.id };
        this.mayChange(spec, 'change', doc, ctx, { moved: true });
        for (const k of Object.keys(doc)) if (doc[k] === null) delete doc[k];
        ops.push({ op: 'upsert', table: physical(appId, table), doc });
        rows.set(key(table, doc.id), doc);
        events.push([table, 'update', doc, Object.keys(changes)]);
        return doc;
      },
      remove: async (table, id) => {
        const spec = await this.tableSpec(appId, table);
        const old = await current(table, String(id));
        this.mayChange(spec, 'remove', old, ctx);
        ops.push({ op: 'delete', table: physical(appId, table), key: String(id) });
        rows.set(key(table, String(id)), null);
        if (old) events.push([table, 'delete', old]);
        return { removed: old ? 1 : 0 };
      },
      // What runs once the writes are stored (a rule's email).
      later: (fn) => { afterwards.push(fn); },
      commit: async () => {
        if (ops.length) {
          await (await this.dbOf(appId)).write(ops.splice(0));
          this.announce('data', { app: appId, tables: [...new Set(events.map((e) => e[0]))] });
          // The taps that led to the writes go with the first of them.
          for (const [i, [table, event, row, changed]] of events.splice(0).entries()) await this.after(appId, table, event, row, { ...ctx, batch: undefined, changed, taps: i ? null : ctx.taps });
        }
        for (const fn of afterwards.splice(0)) await fn();
      },
    };
  }

  // A visitor of an app that lets them add (publish.mjs, access "add") edits
  // and deletes only the rows they added: the insert recorded with them as
  // its person. Throws a 403 otherwise.
  async ownedBy(appId, table, rid, who) {
    const rows = who ? await this.db.query("SELECT person FROM sa_events WHERE app = :a AND kind = 'insert' AND tbl = :t AND rid = :r LIMIT 1", { a: appId, t: table, r: String(rid) }).catch(() => []) : [];
    if (!rows.length || rows[0].person !== String(who).slice(0, 40)) {
      throw Object.assign(new Error('You can change only what you added yourself'), { status: 403 });
    }
  }

  // Tracks the write and runs the functions that wait for it.
  async after(appId, table, event, row, ctx) {
    const view = await this.view(appId);
    // A build's own checks (checks.mjs, ctx.test) are not anyone's use of the app.
    if (!ctx.depth && !ctx.test) {
      const detail = event === 'update' && ctx.changed ? `${table}: ${ctx.changed.join(', ')}` : table;
      const where = { person: ctx.who || ctx.user?.id, v: view.comp.n, session: ctx.session, screen: ctx.screen, tbl: table, rid: row?.id, fields: event === 'update' ? ctx.changed : null, taps: ctx.taps, src: ctx.owner ? 'owner' : null };
      const tracked = Promise.resolve(this.dbOf(appId)).then((db) => rowLabel(db, appId, table, row, tablesOf(view))).catch(() => null)
        .then((label) => this.store.track(appId, event, '', detail, { ...where, label })).catch(() => {});
      // A visitor's insert is how their own rows are told (ownedBy): kept before the answer.
      if (ctx.visitor && event === 'insert') await tracked;
    }
    if ((ctx.depth || 0) >= MAX_DEPTH) return;
    for (const fn of view.functions) {
      const on = fn.spec.on;
      if (!on || on.table !== table || ![].concat(on.event || 'insert').includes(event)) continue;
      if (fn.id === ctx.by) continue;
      // A trigger's failure is the function's, not the write's: the write stands.
      const run = this.call(appId, fn.slug, { row, table, event }, { depth: ctx.depth || 0, trigger: `${event} ${table}`, user: ctx.user || null, test: !!ctx.test, visitor: !!ctx.visitor, pub: !!(ctx.pub || ctx.visitor) })
        .catch((e) => this.log(`trigger ${fn.id}: ${e.message}`));
      this.pending.add(run);
      run.finally(() => this.pending.delete(run));
    }
  }

  // Waits until the triggers that writes started (and the triggers those
  // started) have run.
  async settle() {
    while (this.pending.size) await Promise.allSettled([...this.pending]);
  }

  // A piece that failed: recorded, and sent on to the server, which may ask
  // the forge to repair it. A failure a visitor of a published app caused
  // (visitor) is only recorded: what a stranger sends never starts a build
  // or reaches the model's prompt.
  reportError(appId, capId, message, where, { visitor = false } = {}) {
    this.store.track(appId, 'error', capId, `${visitor ? 'for a visitor, ' : ''}${where}: ${message}`).catch(() => {});
    if (!visitor) this.emit('error', { app: appId, cap: capId, message, where });
  }

  // An error a view's frame reports (POST /api/apps/<id>/error). From code
  // the app no longer runs it is stale; from a visitor it is only recorded.
  // One that says a call was not answered (the network, the engine or
  // the server restarting: unanswered()) is no fault of the code: it is
  // neither recorded nor repaired. A browser of an older runtime still
  // sends them.
  async browserError(appId, capId, message, { visitor = false } = {}) {
    if (unanswered(message)) return { stale: false, sent: false, transient: true };
    const live = await this.store.resolve(appId).catch(() => null);
    if (!live?.comp.caps.includes(capId)) return { stale: true };
    this.reportError(appId, capId, String(message || '').slice(0, 1500), 'in the browser', { visitor });
    return { stale: false, sent: !visitor };
  }
}
