// Declared access (engine/access.mjs): the statements, the decisions, the
// SQL a scope makes, and a published app over a real chloedb-server, where
// the engine (not the screens) keeps visitors and members to what the app
// declares.

import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { join } from 'node:path';
import { parseAllow, allowText, decide, wantsAccess, checkAccess } from '../engine/access.mjs';
import { rewriteSql } from '../engine/sql.mjs';
import { Engine, personName } from '../engine/engine.mjs';
import { accessProblems } from '../engine/forge.mjs';

const status = (e) => e && e.status;

test('ALLOW statements: what, on which table, to whom, which rows', () => {
  assert.deepEqual(parseAllow('ALLOW READ, WRITE ON receivables TO members'), { table: 'receivables', actions: ['read', 'add', 'change', 'remove'], who: 'members', scope: [] });
  assert.deepEqual(parseAllow('allow all on notes to members where owner_id = :user_id;'), { table: 'notes', actions: ['read', 'add', 'change', 'remove'], who: 'members', scope: [{ col: 'owner_id', by: 'user' }] });
  assert.deepEqual(parseAllow('ALLOW READ ON invoices TO members WHERE company_id = :company_id OR owner_id = :user.id').scope, [{ col: 'company_id', by: 'company' }, { col: 'owner_id', by: 'user' }]);
  assert.equal(allowText(parseAllow('ALLOW SELECT ON t TO everyone')), 'ALLOW READ ON t TO everyone');
  assert.equal(allowText(parseAllow('ALLOW ALL ON t TO admins WHERE owner_id = :user.id')), 'ALLOW ALL ON t TO admins WHERE owner_id = :user_id');
  assert.throws(() => parseAllow('ALLOW READ ON t TO strangers'), /say everyone, members, admins or owner/);
  assert.throws(() => parseAllow('ALLOW PEEK ON t TO members'), /PEEK is not READ/);
  assert.throws(() => parseAllow("ALLOW READ ON t TO members WHERE status = 'open'"), /WHERE takes <column> = :user_id/);
  assert.deepEqual(checkAccess(['ALLOW READ ON nope TO members', 'ALLOW ALL ON t TO members WHERE owner_id = :user_id'], new Map([['t', new Set(['id', 'title'])]])),
    ['access: ALLOW READ ON nope TO members names the table nope, which the app does not have', 'access: ALLOW ALL ON t WHERE owner_id = :user_id: t has no column owner_id (add it to the table, a keyword column)'.replace('ALLOW ALL ON t WHERE', 'ALLOW ALL ON t TO members WHERE')]);
});

test('decisions: roles include the ones above them, scopes need a person or a company, the studio has every row', () => {
  const spec = { name: 'notes', access: ['ALLOW READ ON notes TO members', 'ALLOW WRITE ON notes TO members WHERE owner_id = :user_id', 'ALLOW ALL ON notes TO admins'] };
  const visitor = { role: 'visitor', user: null, company: null };
  const member = { role: 'member', user: { id: 'p1' }, company: null };
  const admin = { role: 'admin', user: { id: 'p2' }, company: null };
  const owner = { role: 'owner', user: { id: 'p3' }, company: null };
  assert.throws(() => decide(spec, 'read', visitor), (e) => status(e) === 401 && /Sign in first: only the app's signed-in members can see notes/.test(e.message));
  assert.deepEqual(decide(spec, 'read', member), { all: true });
  assert.deepEqual(decide(spec, 'change', member), { scope: [{ col: 'owner_id', by: 'user' }] });
  assert.deepEqual(decide(spec, 'change', admin), { all: true });
  assert.deepEqual(decide(spec, 'remove', owner), { all: true });
  assert.deepEqual(decide(spec, 'read', null), { all: true }, 'the studio');
  assert.deepEqual(decide({ name: 'open' }, 'remove', visitor), { all: true }, 'a table with no statement is as before');
  const ownerOnly = { name: 'menu', access: ['ALLOW READ ON menu TO everyone', 'ALLOW WRITE ON menu TO owner'] };
  assert.deepEqual(decide(ownerOnly, 'read', visitor), { all: true });
  assert.throws(() => decide(ownerOnly, 'add', admin), (e) => status(e) === 403 && /Only the app's owner can add to menu/.test(e.message));
  const firm = { name: 'invoices', access: ['ALLOW ALL ON invoices TO members WHERE company_id = :company_id'] };
  assert.throws(() => decide(firm, 'read', member), (e) => status(e) === 403 && /Join or choose a company first/.test(e.message));
  assert.deepEqual(decide(firm, 'read', { ...member, company: 'c1' }), { scope: [{ col: 'company_id', by: 'company' }] });
});

test('a scope joins the WHERE of every SELECT that reads the table, so search and joins keep working', () => {
  const scope = (n) => (n === 'notes' ? { sql: (a, { left }) => (left ? `(${a}.owner_id = :sa_user_id OR ${a}.id IS NULL)` : `(${a}.owner_id = :sa_user_id)`) } : null);
  const rw = (q) => rewriteSql(q, ['notes', 'tags'], (t) => `app_x_${t}`, { scope }).sql;
  assert.equal(rw('SELECT * FROM notes'), 'SELECT * FROM app_x_notes AS notes WHERE (notes.owner_id = :sa_user_id)');
  assert.equal(rw("SELECT title FROM notes WHERE MATCH(title, :q) OR x = 1 ORDER BY bm25(title, :q) DESC LIMIT 5"),
    'SELECT title FROM app_x_notes AS notes WHERE (notes.owner_id = :sa_user_id) AND ( MATCH(title, :q) OR x = 1) ORDER BY bm25(title, :q) DESC LIMIT 5');
  assert.equal(rw('SELECT n.title FROM notes n JOIN tags t ON t.id = n.tag GROUP BY n.title'), 'SELECT n.title FROM app_x_notes n JOIN app_x_tags t ON t.id = n.tag WHERE (n.owner_id = :sa_user_id) GROUP BY n.title');
  assert.equal(rw('SELECT t.name FROM tags t LEFT JOIN notes AS n ON n.tag = t.id'), 'SELECT t.name FROM app_x_tags t LEFT JOIN app_x_notes AS n ON n.tag = t.id WHERE (n.owner_id = :sa_user_id OR n.id IS NULL)');
  assert.equal(rw('SELECT * FROM tags WHERE id IN (SELECT tag FROM notes) LIMIT 3'), 'SELECT * FROM app_x_tags AS tags WHERE id IN (SELECT tag FROM app_x_notes AS notes WHERE (notes.owner_id = :sa_user_id)) LIMIT 3');
  assert.equal(rw('WITH x AS (SELECT * FROM notes) SELECT * FROM x UNION SELECT * FROM notes LIMIT 3'),
    'WITH x AS (SELECT * FROM app_x_notes AS notes WHERE (notes.owner_id = :sa_user_id)) SELECT * FROM x UNION SELECT * FROM app_x_notes AS notes WHERE (notes.owner_id = :sa_user_id) LIMIT 3');
  assert.equal(rw('SELECT * FROM tags'), 'SELECT * FROM app_x_tags AS tags');
  assert.throws(() => rw('SELECT * FROM tags t RIGHT JOIN notes n ON n.tag = t.id'), /RIGHT or FULL JOIN/);
  assert.throws(() => rw("SELECT * FROM tags WHERE REACHABLE(notes, 'a')"), /cannot be walked/);
});

test(':user_id with nobody signed in is "sign in first", never an unbound parameter; a page cannot send someone else\'s', async () => {
  const sent = [];
  const engine = new Engine({ query: async (sql, p) => { sent.push(p); return []; }, explain: async () => ({ ir: {} }) }, { log: () => {} });
  engine.view = async () => ({ tables: [{ spec: { name: 'notes', columns: [{ name: 'owner_id', type: 'keyword' }] } }], views: [], functions: [], comp: { n: 1 }, app: { id: 'a1' } });
  const q = 'SELECT * FROM notes WHERE owner_id = :user_id';
  await assert.rejects(engine.sql('a1', q, {}), (e) => e.status === 401 && /^Sign in first/.test(e.message));
  await assert.rejects(engine.sql('a1', q, {}, { as: { role: 'visitor', user: null } }), (e) => e.status === 401);
  await assert.rejects(engine.sql('a1', q, { user_id: 'someone' }, { as: { role: 'visitor', user: null } }), (e) => e.status === 401, 'a visitor cannot say who they are');
  await engine.sql('a1', q, { user_id: 'bob' }, { as: { role: 'member', user: { id: 'alice' } } });
  assert.equal(sent.at(-1).user_id, 'alice', 'bound to the signed-in person');
  await engine.sql('a1', q, {}, { user: { id: 'owner1' } });
  assert.equal(sent.at(-1).user_id, 'owner1', 'the studio binds its person when the call gives none');
  await assert.rejects(engine.sql('a1', 'SELECT * FROM notes WHERE owner_id = :company_id', {}, { as: { role: 'member', user: { id: 'a' }, company: null } }), /Join or choose a company first/);
  // A function that reads :user_id for a visitor: the same clean refusal, through call.
  engine.view = async () => ({ tables: [{ spec: { name: 'notes', columns: [{ name: 'owner_id', type: 'keyword' }] } }], views: [], helpers: [],
    functions: [{ id: 'a1/function/mine@1', slug: 'mine', spec: {}, code: "function run(input, app) { return app.sql('SELECT * FROM notes WHERE owner_id = :user_id', { user_id: app.user ? app.user.id : undefined }); }" }], comp: { n: 1 }, app: { id: 'a1' } });
  engine.store = { track: async () => {} };
  await assert.rejects(engine.call('a1', 'mine', {}, { visitor: true, as: { role: 'visitor', user: null } }), (e) => e.status === 401 && /Sign in first/.test(e.message) && !/unbound/.test(e.message));
});

test('a person without a name is shown by their email, else as "you"', async () => {
  assert.equal(personName({ name: 'Ana', email: 'a@x.io' }), 'Ana');
  assert.equal(personName({ name: '', email: 'a@x.io' }), 'a@x.io');
  assert.equal(personName({ email: null }), 'you');
  assert.equal(personName(null), 'you');
  const { Services } = await import('../engine/services.mjs');
  const db = { get: async (t) => (t === 'sa_people' ? [{ id: 'p1', email: 'owner@x.io', name: null }] : [{ id: 'a#p1', role: 'owner' }]), query: async () => [{ person: 'p1', role: 'owner' }] };
  const s = new Services({ db, store: {} }, { filesDir: '/tmp' });
  assert.equal((await s.user('a', 'p1', { join: false })).name, 'owner@x.io');
  assert.equal((await s.people('a'))[0].name, 'owner@x.io');
});

test('the forge refuses a build that asks for signed-in people only and declares no access', () => {
  const view = { tables: [{ spec: { name: 'receivables', columns: [{ name: 'customer', type: 'text' }] } }] };
  assert.ok(wantsAccess('add authentication: only signed-in users can read or change the data'));
  assert.ok(wantsAccess('each company sees its own invoices'));
  assert.ok(!wantsAccess('a reading list with tags'));
  const ask = 'add authentication: only signed-in users can read or change the data';
  assert.match(accessProblems({ views: [] }, view, ask)[0], /receivables declares no access/);
  assert.deepEqual(accessProblems({ access: ['ALLOW ALL ON receivables TO members'] }, view, ask), []);
  assert.deepEqual(accessProblems({}, { tables: [{ spec: { name: 'receivables', access: ['ALLOW ALL ON receivables TO members'] } }] }, ask), [], 'declared before');
  assert.deepEqual(accessProblems({}, view, 'make the header blue'), []);
  assert.match(accessProblems({ access: ['ALLOW ALL ON receivables TO members WHERE owner_id = :user_id'] }, view, '')[0], /no column owner_id/);
});

// ---- a published app over a real chloedb-server (ports 7950 and 7960)

test('a published app: visitors, members, row scopes by person and by company, enforced by the engine', async (t) => {
  const { findServerBinary, startServer } = await import('../engine/strata.mjs');
  const { Store } = await import('../engine/store.mjs');
  const bin = findServerBinary(join(import.meta.dirname, '..', '..'));
  if (!bin) {
    if (process.env.STRATA_APPS_NEED_SERVER === '1') throw new Error('no chloedb-server binary');
    return t.skip('no chloedb-server binary');
  }
  const dir = mkdtempSync('/tmp/sa-access-');
  const { child: strata, client } = await startServer({ bin, db: join(dir, 'apps.strata'), listen: '127.0.0.1:7950', log: () => {} });
  let engineProc = null;
  try {
    const store = new Store(client, () => {});
    await store.init();
    const app = await store.createApp({ name: 'Ledger' });
    const tables = [
      { name: 'receivables', columns: [{ name: 'customer', type: 'text' }, { name: 'amount', type: 'float' }], access: ['ALLOW ALL ON receivables TO members'] },
      { name: 'notes', columns: [{ name: 'title', type: 'text' }, { name: 'owner_id', type: 'keyword' }], access: ['ALLOW ALL ON notes TO members WHERE owner_id = :user_id'] },
      { name: 'invoices', columns: [{ name: 'title', type: 'text' }, { name: 'company_id', type: 'keyword' }], access: ['ALLOW ALL ON invoices TO members WHERE company_id = :company_id'] },
      { name: 'products', columns: [{ name: 'name', type: 'text' }, { name: 'owner_id', type: 'keyword' }] },
    ];
    const caps = [];
    const have = new Set(await store.tableNames());
    for (const spec of tables) {
      await store.ensureTable(app, spec, have);
      caps.push(await store.putCapability({ app, slug: spec.name, kind: 'table', title: spec.name, spec, code: '' }));
    }
    caps.push(await store.putCapability({ app, slug: 'mine', kind: 'function', title: 'Mine', spec: { on: null },
      code: "function run(input, app) { return app.sql('SELECT name FROM products WHERE owner_id = :user_id', { user_id: app.user ? app.user.id : undefined }); }" }));
    await store.writeComposition(app, caps, 'first', 'a ledger');
    await client.write([{ op: 'insert', table: `app_${app}_receivables`, doc: { id: 'r1', customer: 'Acme', amount: 120 } },
      { op: 'insert', table: `app_${app}_products`, doc: { id: 'p1', name: 'Widget', owner_id: 'x' } }]);
    await store.putSetting(app, 'publish', JSON.stringify({ on: true, name: 'ledger', access: 'change' }));

    const TOKEN = 'access-test-token';
    engineProc = spawn(process.execPath, [join(import.meta.dirname, '..', 'engine', 'server.mjs'), '--port', '7960', '--strata', 'http://127.0.0.1:7950', '--data', join(dir, 'engine')],
      { env: { ...process.env, STRATA_APP_TOKEN: TOKEN, STRATA_APPS_SCHEDULES: 'off', STRATA_APPS_AUTOREPAIR: '0', STRATA_APPS_TILES: 'off', STRATA_APPS_GEOCODER: 'off' }, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    engineProc.stdout.on('data', (d) => { out += d; });
    engineProc.stderr.on('data', (d) => { out += d; });
    const base = 'http://127.0.0.1:7960';
    for (let i = 0; ; i++) {
      try { if ((await fetch(`${base}/healthz`)).ok) break; } catch { /* starting */ }
      if (i > 600 || engineProc.exitCode !== null) throw new Error(`the engine did not start:\n${out.slice(-2000)}`);
      await new Promise((r) => setTimeout(r, 100));
    }
    const call = async (path, body, { cookie, studio, company } = {}) => {
      const headers = { 'content-type': 'application/json', ...(cookie ? { cookie } : {}), ...(studio ? { authorization: `Bearer ${TOKEN}` } : {}), ...(company ? { 'x-sa-company': company } : {}) };
      const res = await fetch(`${base}/api/apps/${app}${path}`, { method: body === undefined ? 'GET' : 'POST', headers, body: body === undefined ? undefined : JSON.stringify(body) });
      return { status: res.status, body: await res.json().catch(() => ({})), cookie: (res.headers.get('set-cookie') || '').split(';')[0] };
    };
    const sql = (q, opts, params) => call('/sql', { sql: q, params }, opts);

    // A visitor (nobody signed in) is refused a members-only table: 401, said plainly.
    const v = await sql('SELECT customer FROM receivables');
    assert.equal(v.status, 401, JSON.stringify(v.body));
    assert.match(v.body.error, /^Sign in first/);
    assert.equal((await call('/insert', { table: 'receivables', row: { customer: 'Mallory' } })).status, 401);
    // A table that declares nothing is as before.
    assert.deepEqual((await sql('SELECT name FROM products')).body.rows, [{ name: 'Widget' }]);
    // A function that needs :user_id, called by a visitor: a clean refusal.
    const fn = await call('/call', { name: 'mine', input: {} });
    assert.equal(fn.status, 401, JSON.stringify(fn.body));
    assert.match(fn.body.error, /Sign in first/);
    assert.doesNotMatch(fn.body.error, /unbound|strata/);

    // Members sign up on the app's own page.
    const alice = await call('/signup', { email: 'alice@example.com', name: 'Alice', password: 'correct horse' });
    const bob = await call('/signup', { email: 'bob@example.com', name: '', password: 'battery staple' });
    assert.equal(alice.status, 200, JSON.stringify(alice.body));
    const A = { cookie: alice.cookie };
    const B = { cookie: bob.cookie };
    const aliceId = alice.body.user.id;
    const bobId = bob.body.user.id;
    assert.equal(bob.body.user.name, 'bob', 'a name from the email when none is given');
    assert.deepEqual((await sql('SELECT customer FROM receivables', A)).body.rows, [{ customer: 'Acme' }], 'a member reads it');
    assert.equal((await call('/insert', { table: 'receivables', row: { customer: 'Globex', amount: 5 } }, A)).status, 200);

    // Rows scoped by person: the studio writes both people's notes.
    for (const [id, title, owner] of [['n1', 'Alice 1', aliceId], ['n2', 'Bob 1', bobId]]) {
      const r = await call('/insert', { table: 'notes', row: { id, title, owner_id: owner } }, { studio: true });
      assert.equal(r.status, 200, JSON.stringify(r.body));
    }
    assert.equal((await sql('SELECT title FROM notes', { studio: true })).body.rows.length, 2, 'the studio sees every row');
    assert.deepEqual((await sql('SELECT title FROM notes ORDER BY title', A)).body.rows, [{ title: 'Alice 1' }]);
    assert.deepEqual((await sql('SELECT title FROM notes ORDER BY title', B)).body.rows, [{ title: 'Bob 1' }]);
    assert.deepEqual((await sql('SELECT title FROM notes WHERE owner_id = :user_id', A, { user_id: bobId })).body.rows, [{ title: 'Alice 1' }], ':user_id is the engine\'s, not the page\'s');
    assert.deepEqual((await sql('SELECT COUNT(*) AS n FROM notes', A)).body.rows, [{ n: 1 }]);
    assert.equal((await sql('SELECT title FROM notes', {})).status, 401);
    const added = await call('/insert', { table: 'notes', row: { title: 'Alice 2' } }, A);
    assert.equal(added.status, 200, JSON.stringify(added.body));
    assert.equal(added.body.row.owner_id, aliceId, 'the scope column is filled');
    assert.equal((await call('/insert', { table: 'notes', row: { title: 'Forged', owner_id: bobId } }, A)).status, 403);
    assert.equal((await call('/update', { table: 'notes', id: 'n2', fields: { title: 'Hijacked' } }, A)).status, 403);
    assert.equal((await call('/remove', { table: 'notes', id: 'n2' }, A)).status, 403);
    assert.equal((await call('/update', { table: 'notes', id: 'n1', fields: { owner_id: bobId } }, A)).status, 403, 'a row is not moved out of the scope');
    assert.equal((await call('/update', { table: 'notes', id: 'n1', fields: { title: 'Alice 1b' } }, A)).status, 200);
    assert.equal((await call('/remove', { table: 'notes', id: 'n2' }, B)).status, 200);

    // Rows scoped by company: the engine keeps who works where.
    assert.equal((await sql('SELECT title FROM invoices', A)).status, 403, 'no company yet');
    const acme = (await call('/companies', { name: 'Acme' }, A)).body.company;
    assert.equal(acme.role, 'admin');
    for (const [id, title, company] of [['i1', 'Acme invoice', acme.id], ['i2', 'Other invoice', 'someone-else']]) await call('/insert', { table: 'invoices', row: { id, title, company_id: company } }, { studio: true });
    assert.deepEqual((await sql('SELECT title FROM invoices', A)).body.rows, [{ title: 'Acme invoice' }]);
    assert.equal((await sql('SELECT title FROM invoices', B)).status, 403);
    assert.equal((await call('/companies/members', { company: acme.id, email: 'alice@example.com' }, B)).status, 403, 'only its admins add people');
    assert.equal((await call('/companies/members', { company: acme.id, email: 'bob@example.com', role: 'member' }, A)).status, 200);
    assert.deepEqual((await sql('SELECT title FROM invoices', B)).body.rows, [{ title: 'Acme invoice' }]);
    const beta = (await call('/companies', { name: 'Beta' }, B)).body.company;
    assert.deepEqual((await sql('SELECT title FROM invoices', { ...B, company: beta.id })).body.rows, [], 'the company chosen now');
    const inv = await call('/insert', { table: 'invoices', row: { title: 'Beta invoice' } }, { ...B, company: beta.id });
    assert.equal(inv.body.row.company_id, beta.id);
    assert.deepEqual((await sql('SELECT title FROM invoices', { ...B, company: 'not-mine' })).body.rows, [{ title: 'Acme invoice' }], 'a company one is not in falls back to one of theirs');
    const listed = (await call('/companies', undefined, B)).body;
    assert.deepEqual(listed.companies.map((c) => c.name).sort(), ['Acme', 'Beta']);
  } finally {
    if (engineProc) { engineProc.kill('SIGTERM'); await new Promise((r) => { if (engineProc.exitCode !== null) r(); else engineProc.on('exit', r); }); }
    strata.kill();
  }
});
