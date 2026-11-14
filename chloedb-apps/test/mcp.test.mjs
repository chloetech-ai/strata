// The engine's MCP server (engine/mcp.mjs), spawned over stdio against a
// stub of the engine's HTTP API: the protocol (initialize, tools/list, ping,
// tools/call, errors) and each tool's call. No model, no ChloeDB.

import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import readline from 'node:readline';
import os from 'node:os';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { readOnlySql, TOOLS } from '../engine/mcp.mjs';

const MCP = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'engine', 'mcp.mjs');
const TOKEN = 'agent-token-for-the-test';

// The stub engine: records each call, answers like the real routes.
function stubEngine() {
  const calls = [];
  let seq = 5;
  const reqs = [{ id: 'r1', status: 'done', text: 'first', summary: 'built it', comp: 1, error: '' }];
  const srv = http.createServer(async (req, res) => {
    let raw = '';
    for await (const c of req) raw += c;
    const url = new URL(req.url, 'http://x');
    const body = raw ? JSON.parse(raw) : undefined;
    calls.push({ method: req.method, path: url.pathname, query: Object.fromEntries(url.searchParams), body, auth: req.headers.authorization });
    const send = (code, j) => { res.writeHead(code, { 'content-type': 'application/json' }); res.end(JSON.stringify(j)); };
    // The engine's login (login.mjs): the agent token, or nothing.
    if (req.headers.authorization !== `Bearer ${TOKEN}`) return send(401, { error: 'that bearer token is not this engine\'s' });
    const p = url.pathname;
    if (p === '/api/apps' && req.method === 'GET') return send(200, { apps: [{ id: 'abc123', name: 'Counter', head: 2, created: 't', cost: { usd: 0, builds: 1 }, extra: 'x' }] });
    if (p === '/api/apps' && req.method === 'POST') return send(200, { app: 'new1', request: 'r9' });
    if (p === '/api/apps/abc123' && req.method === 'GET') {
      return send(200, { app: { id: 'abc123', name: 'Counter', head: 2, description: 'd' }, version: { n: 2, live: 2, summary: 's2' },
        versions: [{ n: 2, summary: 's2', created: 't2', caps: ['big'] }, { n: 1, summary: 's1', created: 't1' }],
        tables: [{ name: 'counts', rows: 3, columns: [{ name: 'n', type: 'number' }] }],
        views: [{ kind: 'view', slug: 'home', title: 'Home', version: 2, code: 'HUGE', spec: { statement: 'CREATE SCREEN home ...' } }],
        functions: [], components: [], helpers: [], people: [{ email: 'secret@x' }],
        requests: reqs, suggestions: [{ id: 's1', text: 'Add a reset', why: 'w' }], running: null, publish: { on: false } });
    }
    if (p === '/api/apps/abc123/requests') { reqs.unshift({ id: 'r2', status: 'running', text: body.text }); setTimeout(() => { seq++; Object.assign(reqs[0], { status: 'done', comp: 3, summary: 'added it' }); stub.event = { seq, type: 'done', data: { app: 'abc123', request: 'r2', n: 3, summary: 'added it' } }; }, 800); return send(200, { request: 'r2' }); }
    if (p === '/api/events') {
      const after = Number(url.searchParams.get('after'));
      const wait = Number(url.searchParams.get('wait'));
      const t0 = Date.now();
      while (!(stub.event && stub.event.seq > after) && Date.now() - t0 < Math.min(wait, 2000)) await new Promise((r) => setTimeout(r, 50));
      return send(200, { seq, events: stub.event && stub.event.seq > after ? [stub.event] : [] });
    }
    if (p === '/api/apps/abc123/source') return send(200, { name: 'Counter', sql: 'CREATE TABLE counts (n NUMBER);' });
    if (p === '/api/apps/abc123/screens/home') return send(200, { n: 3 });
    if (p === '/api/apps/abc123/sql') return send(200, { rows: [{ n: 1 }] });
    if (p === '/api/apps/abc123/restore') return send(200, { head: body.n });
    if (p === '/api/apps/abc123/publish') return send(200, { on: body.on, access: body.access });
    if (p === '/api/library') return send(200, { capabilities: [{ slug: 'kanban' }] });
    if (p === '/api/apps/nope00') return send(404, { error: 'no app nope00' });
    send(404, { error: 'not found' });
  });
  const stub = { calls, srv, event: null };
  return new Promise((ok) => srv.listen(0, '127.0.0.1', () => { stub.url = `http://127.0.0.1:${srv.address().port}`; ok(stub); }));
}

// The MCP server as a child process; rpc(method, params) sends one request.
function spawnMcp(url, { token = TOKEN, args = [] } = {}) {
  const env = { ...process.env, STRATA_APPS_AGENT_TOKEN: token };
  if (token === null) delete env.STRATA_APPS_AGENT_TOKEN;
  const child = spawn(process.execPath, [MCP, '--url', url, ...args], { stdio: ['pipe', 'pipe', 'pipe'], env });
  const waiting = new Map();
  const lines = [];
  readline.createInterface({ input: child.stdout }).on('line', (l) => {
    lines.push(l);
    const m = JSON.parse(l);
    const w = waiting.get(m.id);
    if (w) { waiting.delete(m.id); w(m); }
  });
  let n = 0;
  const send = (obj) => child.stdin.write(`${typeof obj === 'string' ? obj : JSON.stringify(obj)}\n`);
  const rpc = (method, params) => new Promise((ok) => { const id = ++n; waiting.set(id, ok); send({ jsonrpc: '2.0', id, method, params }); });
  const next = () => new Promise((ok) => { const before = lines.length; const t = setInterval(() => { if (lines.length > before) { clearInterval(t); ok(JSON.parse(lines[before])); } }, 10); });
  return { child, rpc, send, next, lines, tool: async (name, args) => (await rpc('tools/call', { name, arguments: args })).result };
}
const body = (r) => JSON.parse(r.content[0].text);

test('MCP: the protocol over stdio, and each tool as a call to the engine', async (t) => {
  const stub = await stubEngine();
  const m = spawnMcp(stub.url);
  t.after(() => { m.child.kill(); stub.srv.close(); });

  const init = await m.rpc('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'test', version: '1' } });
  assert.equal(init.jsonrpc, '2.0');
  assert.equal(init.result.protocolVersion, '2025-06-18');
  assert.ok(init.result.capabilities.tools);
  assert.equal(init.result.serverInfo.name, 'chloedb-apps');
  const old = await m.rpc('initialize', { protocolVersion: '1999-01-01' });
  assert.equal(old.result.protocolVersion, '2025-06-18', 'an unknown version gets ours');
  m.send({ jsonrpc: '2.0', method: 'notifications/initialized' });
  assert.deepEqual((await m.rpc('ping')).result, {});

  const list = (await m.rpc('tools/list')).result.tools;
  assert.deepEqual(list.map((x) => x.name), ['app_list', 'app_create', 'app_state', 'app_request', 'app_wait', 'piece_source', 'screen_edit', 'data_query', 'version_list', 'version_restore', 'library_search', 'publish']);
  for (const x of list) { assert.equal(x.inputSchema.type, 'object'); assert.ok(x.description.length < 300, `${x.name}: a short description`); }
  assert.match(list.find((x) => x.name === 'app_request').description, /Prefer this for most changes/);
  assert.match(list.find((x) => x.name === 'screen_edit').description, /small exact edits/);

  // Errors: an unknown method, an unknown tool, a line that is not JSON.
  assert.equal((await m.rpc('nope/never')).error.code, -32601);
  assert.equal((await m.rpc('tools/call', { name: 'rm_rf', arguments: {} })).error.code, -32602);
  m.send('{not json');
  assert.equal((await m.next()).error.code, -32700);

  // The tools.
  const apps = body(await m.tool('app_list', {}));
  assert.deepEqual(apps.apps, [{ id: 'abc123', name: 'Counter', head: 2, created: 't', cost: { usd: 0, builds: 1 } }]);

  const made = await m.tool('app_create', { description: 'A simple counter with a plus button', provider: 'codex' });
  assert.equal(made.isError, false);
  assert.deepEqual(body(made), { app: 'new1', request: 'r9' });
  assert.deepEqual(stub.calls.at(-1), { method: 'POST', path: '/api/apps', query: {}, auth: `Bearer ${TOKEN}`, body: { description: 'A simple counter with a plus button', provider: 'codex' } });
  assert.equal((await m.tool('app_create', { description: 'x', provider: 'gpt' })).isError, true);

  const st = body(await m.tool('app_state', { app: 'abc123' }));
  assert.equal(st.name, 'Counter');
  assert.equal(st.head, 2);
  assert.deepEqual(st.versions.map((v) => v.n), [2, 1]);
  assert.deepEqual(st.tables, [{ name: 'counts', rows: 3, columns: ['n number'] }]);
  assert.deepEqual(st.screens, [{ kind: 'view', slug: 'home', title: 'Home', version: 2, statement: true }]);
  assert.equal(st.suggestions[0].text, 'Add a reset');
  assert.ok(!JSON.stringify(st).includes('HUGE') && !JSON.stringify(st).includes('secret@x'), 'trimmed: no code, no people');

  // Ids are checked before anything is sent.
  const n0 = stub.calls.length;
  const badId = await m.tool('app_state', { app: '../api/status' });
  assert.equal(badId.isError, true);
  assert.match(badId.content[0].text, /app id/);
  assert.equal(stub.calls.length, n0);
  // The engine's error comes back as the tool's error.
  const missing = await m.tool('app_state', { app: 'nope00' });
  assert.equal(missing.isError, true);
  assert.match(missing.content[0].text, /404 no app nope00/);

  // A request, then waiting for it: its done event.
  assert.deepEqual(body(await m.tool('app_request', { app: 'abc123', text: 'Add a minus button' })), { request: 'r2' });
  assert.deepEqual(stub.calls.at(-1).body, { text: 'Add a minus button' });
  const done = body(await m.tool('app_wait', { app: 'abc123', request: 'r2', timeout_s: 10 }));
  assert.deepEqual(done, { request: 'r2', status: 'done', version: 3, summary: 'added it' });
  assert.ok(stub.calls.some((c) => c.path === '/api/events' && c.query.app === 'abc123' && Number(c.query.after) > 0), 'long-polls the events after the counter it read');
  // A request already ended is answered from its row.
  assert.equal(body(await m.tool('app_wait', { app: 'abc123', request: 'r1' })).summary, 'built it');

  assert.match(body(await m.tool('piece_source', { app: 'abc123' })).sql, /CREATE TABLE/);
  assert.deepEqual(body(await m.tool('screen_edit', { app: 'abc123', slug: 'home', statement: 'CREATE SCREEN home ...', note: 'relabel' })), { n: 3 });
  assert.deepEqual(stub.calls.at(-1), { method: 'POST', path: '/api/apps/abc123/screens/home', query: {}, auth: `Bearer ${TOKEN}`, body: { statement: 'CREATE SCREEN home ...', note: 'relabel' } });
  assert.equal((await m.tool('screen_edit', { app: 'abc123', slug: '../x', statement: 's' })).isError, true);

  assert.deepEqual(body(await m.tool('data_query', { app: 'abc123', sql: 'SELECT n FROM counts' })), { rows: [{ n: 1 }] });
  const n1 = stub.calls.length;
  const write = await m.tool('data_query', { app: 'abc123', sql: 'DELETE FROM counts' });
  assert.equal(write.isError, true);
  assert.equal(stub.calls.length, n1, 'a write is refused before it is sent');

  assert.deepEqual(body(await m.tool('version_list', { app: 'abc123' })).versions.map((v) => v.n), [2, 1]);
  assert.deepEqual(body(await m.tool('version_restore', { app: 'abc123', n: 1 })), { head: 1 });
  assert.equal((await m.tool('version_restore', { app: 'abc123', n: 'one' })).isError, true);

  assert.deepEqual(body(await m.tool('library_search', { q: 'kanban board' })), { capabilities: [{ slug: 'kanban' }] });
  assert.equal(stub.calls.at(-1).query.q, 'kanban board');

  // Publishing puts the app on the internet: confirm at every access.
  const np = stub.calls.length;
  for (const access of ['look', 'add']) {
    const no = await m.tool('publish', { app: 'abc123', access });
    assert.equal(no.isError, true, access);
    assert.match(no.content[0].text, /confirm: true/);
  }
  assert.equal(stub.calls.length, np, 'refused before it is sent');
  assert.deepEqual(body(await m.tool('publish', { app: 'abc123', access: 'look', confirm: true })), { on: true, access: 'look' });
  assert.deepEqual(body(await m.tool('publish', { app: 'abc123', access: 'look', on: false })), { on: false, access: 'look' }, 'unpublishing needs no confirm');
  const n2 = stub.calls.length;
  const change = await m.tool('publish', { app: 'abc123', access: 'change' });
  assert.equal(change.isError, true);
  assert.match(change.content[0].text, /confirm: true/);
  assert.equal(stub.calls.length, n2);
  assert.deepEqual(body(await m.tool('publish', { app: 'abc123', access: 'change', confirm: true })), { on: true, access: 'change' });
  assert.equal((await m.tool('publish', { app: 'abc123', access: 'all' })).isError, true);

  // Only protocol on stdout.
  for (const l of m.lines) assert.equal(JSON.parse(l).jsonrpc, '2.0');
  // Every call carried the agent token.
  assert.ok(stub.calls.length > 10 && stub.calls.every((c) => c.auth === `Bearer ${TOKEN}`));
});

test('MCP: it does not start without the agent token, and a wrong one is the engine\'s refusal', async (t) => {
  const none = spawn(process.execPath, [MCP, '--url', 'http://127.0.0.1:9', '--token-file', path.join(os.tmpdir(), `sa-no-token-${process.pid}`)], { stdio: ['pipe', 'pipe', 'pipe'], env: (({ STRATA_APPS_AGENT_TOKEN, ...rest }) => rest)(process.env) });
  let err = '';
  none.stderr.on('data', (d) => { err += d; });
  const code = await new Promise((ok) => none.on('close', ok));
  assert.equal(code, 2);
  assert.match(err, /no agent token/);

  // From a file, as the engine writes it.
  const stub = await stubEngine();
  t.after(() => stub.srv.close());
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sa-mcp-token-'));
  writeFileSync(path.join(dir, 'agent.token'), `${TOKEN}\n`, { mode: 0o600 });
  const fromFile = spawnMcp(stub.url, { token: null, args: ['--token-file', path.join(dir, 'agent.token')] });
  t.after(() => fromFile.child.kill());
  assert.equal((await fromFile.tool('app_list', {})).isError, false);

  const wrong = spawnMcp(stub.url, { token: 'stale' });
  t.after(() => wrong.child.kill());
  const r = await wrong.tool('app_list', {});
  assert.equal(r.isError, true);
  assert.match(r.content[0].text, /401/);
});

test('MCP: an engine that is not there is a tool error, not a crash', async (t) => {
  const m = spawnMcp('http://127.0.0.1:9');
  t.after(() => m.child.kill());
  const r = await m.tool('app_list', {});
  assert.equal(r.isError, true);
  assert.match(r.content[0].text, /could not reach the engine/);
  assert.deepEqual((await m.rpc('ping')).result, {});
});

test('data_query takes one read-only SELECT', () => {
  for (const ok of ['SELECT * FROM t', 'select a, replace(b, \'x\', \'y\') from t where c = \'DELETE\';', 'WITH x AS (SELECT 1 AS n) SELECT n FROM x', '-- note\nSELECT 1', 'SELECT "update" FROM t'])
    assert.equal(readOnlySql(ok), null, ok);
  for (const no of ['DELETE FROM t', 'INSERT INTO t VALUES (1)', 'SELECT 1; DROP TABLE t', 'WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x', 'UPDATE t SET a = 1', 'SELECT * INTO u FROM t', 'CREATE TABLE u (a INT)', '', 'SELECT \'unclosed', 'PRAGMA x'])
    assert.ok(readOnlySql(no), no);
  assert.ok(TOOLS.length === 12);
});
