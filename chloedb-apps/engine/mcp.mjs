#!/usr/bin/env node
// The engine as an MCP server, for an agent that builds apps (Claude Code,
// Codex, or any MCP client). A separate process: it speaks the Model Context Protocol over
// stdio (JSON-RPC 2.0, one message per line) and each tool is a thin call to
// a running engine's HTTP API.
//
//   node engine/mcp.mjs --url http://127.0.0.1:7730
//
// --url (or STRATA_APPS_URL) is the engine; nothing is written to stdout but
// protocol messages (logs go to stderr).
//
// It signs in with the engine's agent token, sent as a bearer token on every
// call (audit 4, X-G7), and does not start without one: STRATA_APPS_AGENT_TOKEN,
// or the file --token-file names (default: the engine's DATA/agent.token,
// which the engine writes on its first start, readable by its owner only;
// DATA is STRATA_APPS_DATA or chloedb-apps/data, as for the engine).

import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';

export const PROTOCOL_VERSION = '2025-06-18';
const SUPPORTED_VERSIONS = ['2025-06-18', '2025-03-26', '2024-11-05'];
export const SERVER_INFO = { name: 'chloedb-apps', version: '0.1.0' };

const APP_ID = /^[a-z0-9]{1,40}$/;
const SLUG = /^[a-z0-9-]{1,80}$/;
const ACCESS = ['look', 'add', 'change'];
const PROVIDERS = ['claude', 'codex', 'agy'];

class ToolError extends Error {}
const bad = (m) => { throw new ToolError(m); };
const appId = (a) => (typeof a === 'string' && APP_ID.test(a) ? a : bad('app is an app id: 1 to 40 lowercase letters and digits (see app_list)'));
const text = (t, what) => (typeof t === 'string' && t.trim() ? t : bad(`${what} is required`));
const provider = (p) => (p === undefined || p === null || p === '' ? undefined : PROVIDERS.includes(p) ? p : bad(`provider is one of ${PROVIDERS.join(', ')}`));

// A single read-only statement: SELECT or WITH ... SELECT, nothing that
// writes, defines or runs anything. Comments and quoted text are taken out
// before the words are looked at, so a word in a string does not count.
const WRITES = /\b(INSERT|UPDATE|DELETE|UPSERT|MERGE|DROP|CREATE|ALTER|TRUNCATE|RENAME|GRANT|REVOKE|ATTACH|DETACH|COPY|LOAD|VACUUM|PRAGMA|SET|CALL|EXEC|EXECUTE|BEGIN|COMMIT|ROLLBACK|INTO)\b/i;
export function readOnlySql(sql) {
  if (typeof sql !== 'string' || !sql.trim()) return 'sql is required';
  if (sql.length > 20000) return 'sql is at most 20000 characters';
  const bare = sql
    .replace(/--[^\n]*/g, ' ')
    .replace(/\/\*[\s\S]*?\*\//g, ' ')
    .replace(/'(?:[^']|'')*'/g, "''")
    .replace(/"(?:[^"]|"")*"/g, '""')
    .replace(/`[^`]*`/g, '``')
    .trim()
    .replace(/;\s*$/, '');
  if (/\/\*|'|"/.test(bare.replace(/''|""/g, ''))) return 'the statement has an unclosed comment or quote';
  if (bare.includes(';')) return 'one statement only';
  if (!/^(SELECT|WITH)\b/i.test(bare)) return 'only a read-only SELECT (or WITH ... SELECT) is allowed';
  const w = bare.match(WRITES);
  if (w) return `only a read-only SELECT is allowed (found ${w[1].toUpperCase()})`;
  return null;
}

// The engine's HTTP API.
// token: the engine's agent token, sent as a bearer token.
export function client(base, { fetchFn = fetch, token } = {}) {
  const root = String(base).replace(/\/+$/, '');
  const auth = token ? { authorization: `Bearer ${token}` } : {};
  return async function call(method, pathname, body) {
    let res;
    try {
      res = await fetchFn(`${root}${pathname}`, {
        method,
        headers: body !== undefined ? { ...auth, 'content-type': 'application/json' } : { ...auth },
        body: body !== undefined ? JSON.stringify(body) : undefined,
      });
    } catch (e) {
      throw new ToolError(`could not reach the engine at ${root}: ${e.cause?.message || e.message}`);
    }
    const raw = await res.text();
    let j;
    try { j = raw ? JSON.parse(raw) : {}; } catch { j = { error: raw.slice(0, 500) }; }
    if (!res.ok) throw new ToolError(`${method} ${pathname}: ${res.status} ${j.error || res.statusText}`);
    return j;
  };
}

// What an agent needs of GET /api/apps/:id.
export function trimState(s) {
  const pieces = (list, kind) => (list || []).map((c) => ({ kind: c.kind || kind, slug: c.slug, title: c.title, version: c.version, statement: !!c.spec?.statement }));
  return {
    id: s.app?.id,
    name: s.app?.name,
    description: s.app?.description,
    head: s.version?.live ?? s.app?.head,
    version: s.version ? { n: s.version.n, summary: s.version.summary } : null,
    versions: (s.versions || []).map((v) => ({ n: v.n, summary: v.summary, created: v.created })),
    tables: (s.tables || []).map((t) => ({ name: t.name, rows: t.rows, columns: (t.columns || []).map((c) => (typeof c === 'string' ? c : `${c.name}${c.type ? ` ${c.type}` : ''}`)) })),
    screens: pieces(s.views, 'view'),
    capabilities: [...pieces(s.functions, 'function'), ...pieces(s.components, 'component'), ...pieces(s.helpers, 'helper')],
    running: s.running ? { request: s.running.id, text: s.running.text, elapsed_s: Math.round((s.running.elapsed || 0) / 1000), last: (s.running.lines || []).slice(-3) } : null,
    requests: (s.requests || []).slice(0, 5).map((r) => ({ id: r.id, status: r.status, text: String(r.text || '').slice(0, 200), summary: r.summary, error: r.error || undefined })),
    suggestions: (s.suggestions || []).map((x) => ({ id: x.id, text: x.text, why: x.why })),
    publish: s.publish || null,
  };
}

const str = (description) => ({ type: 'string', description });
const APP = str('App id (from app_list or app_create).');
export const TOOLS = [
  { name: 'app_list', description: 'List the apps on this engine: id, name, live version, build cost.', inputSchema: { type: 'object', properties: {} } },
  { name: 'app_create', description: 'Create an app from a description and start its first build (the engine plans, writes and checks it). Returns {app, request}; then call app_wait.',
    inputSchema: { type: 'object', properties: { description: str('What the app is for, in a sentence or more.'), name: str('Optional name, kept for good.'), provider: { type: 'string', enum: PROVIDERS, description: 'Optional CLI for this build: claude (default), codex, agy.' } }, required: ['description'] } },
  { name: 'app_state', description: 'An app as an agent needs it: name, live version, versions, tables, screens and other pieces, the build running, recent requests, suggestions.',
    inputSchema: { type: 'object', properties: { app: APP }, required: ['app'] } },
  { name: 'app_request', description: 'Ask the engine for a change in plain words; it builds, checks and stores a new version in one call. Prefer this for most changes (new screens, tables, features, fixes). Returns {request}; then call app_wait.',
    inputSchema: { type: 'object', properties: { app: APP, text: str('The change, in plain words.'), provider: { type: 'string', enum: PROVIDERS, description: 'Optional CLI for this build: claude (default), codex, agy.' } }, required: ['app', 'text'] } },
  { name: 'app_wait', description: 'Wait for a build (app_create or app_request) to end. Returns its summary and new version, or its error. Without request, waits for the build running now.',
    inputSchema: { type: 'object', properties: { app: APP, request: str('Request id from app_create or app_request.'), timeout_s: { type: 'number', description: 'How long to wait, 1 to 1800 s (default 600).' } }, required: ['app'] } },
  { name: 'piece_source', description: 'The whole app as one SQL script (every table, screen and function as its CREATE statement). Read it before screen_edit.',
    inputSchema: { type: 'object', properties: { app: APP }, required: ['app'] } },
  { name: 'screen_edit', description: 'Replace one screen with an exact CREATE SCREEN statement, without the model: instant, checked, a new version. Use only for small exact edits (a label, an order, a column); use app_request for anything else.',
    inputSchema: { type: 'object', properties: { app: APP, slug: str('The screen\'s slug (from app_state).'), statement: str('The whole CREATE SCREEN statement for that slug (start from piece_source).'), note: str('Optional: what the edit does, for the history.') }, required: ['app', 'slug', 'statement'] } },
  { name: 'data_query', description: 'Run one read-only SELECT (or WITH ... SELECT) on the app\'s tables. Returns {rows}.',
    inputSchema: { type: 'object', properties: { app: APP, sql: str('One SELECT statement.') }, required: ['app', 'sql'] } },
  { name: 'version_list', description: 'The app\'s versions, newest first, and which one is live.', inputSchema: { type: 'object', properties: { app: APP }, required: ['app'] } },
  { name: 'version_restore', description: 'Make an earlier version live again (its data stays as it is).',
    inputSchema: { type: 'object', properties: { app: APP, n: { type: 'integer', description: 'Version number (from version_list).' } }, required: ['app', 'n'] } },
  { name: 'library_search', description: 'Search the shared library of pieces (screens, components, helpers, patterns) other apps use, or ask how pieces fit together.',
    inputSchema: { type: 'object', properties: { q: str('Words to search for, or a question.') }, required: ['q'] } },
  { name: 'publish', description: 'Open the app to anyone on the internet. access: look (read only), add (visitors add their own rows) or change (visitors change any row). Publishing at any access needs confirm: true, set only once the person you work for has said to publish; on: false unpublishes and needs no confirm.',
    inputSchema: { type: 'object', properties: { app: APP, access: { type: 'string', enum: ACCESS }, on: { type: 'boolean', description: 'Default true.' }, confirm: { type: 'boolean', description: 'Required to publish (on: true), at any access.' } }, required: ['app', 'access'] } },
];

const enc = encodeURIComponent;

// A build's end, from the request's row or the engine's events (done, failed).
async function waitFor(call, app, request, timeoutS, { pollMs = 20000 } = {}) {
  const deadline = Date.now() + timeoutS * 1000;
  const ended = (r) => (r.status === 'done' ? { request: r.id, status: 'done', version: r.comp, summary: r.summary, ms: r.ms, cost: r.cost }
    : r.status === 'failed' ? { request: r.id, status: 'failed', error: r.error } : null);
  let after = 0;
  // The event counter before the request's row is read, so no end is missed.
  const first = await call('GET', `/api/events?after=0&app=${enc(app)}&wait=0`);
  after = first.seq || 0;
  const st = await call('GET', `/api/apps/${enc(app)}`);
  let id = request;
  if (!id) id = st.running?.id || (st.requests || []).find((r) => r.status === 'queued' || r.status === 'running')?.id;
  if (!id) {
    const last = (st.requests || [])[0];
    if (!last) bad('this app has no requests');
    return ended(last) || { request: last.id, status: last.status };
  }
  const row = (st.requests || []).find((r) => r.id === id);
  if (row && ended(row)) return ended(row);
  if (!row && request) {
    // Not in the latest 30: look at the kept events.
    const old = first.events.find((e) => (e.type === 'done' || e.type === 'failed') && e.data?.request === id);
    if (old) return old.type === 'done' ? { request: id, status: 'done', version: old.data.n, summary: old.data.summary } : { request: id, status: 'failed', error: old.data.error };
  }
  while (Date.now() < deadline) {
    const wait = Math.max(0, Math.min(pollMs, deadline - Date.now()));
    const ev = await call('GET', `/api/events?after=${after}&app=${enc(app)}&wait=${wait}`);
    if (ev.seq < after) after = 0; // the engine restarted
    for (const e of ev.events || []) {
      if (e.data?.request !== id) continue;
      if (e.type === 'done') return { request: id, status: 'done', version: e.data.n, summary: e.data.summary };
      if (e.type === 'failed') return { request: id, status: 'failed', error: e.data.error };
    }
    after = Math.max(after, ev.seq || 0);
    if (!(ev.events || []).length) {
      // A quiet stretch: the row may say it ended (an engine restarted meanwhile).
      const again = (await call('GET', `/api/apps/${enc(app)}`)).requests?.find((r) => r.id === id);
      if (again && ended(again)) return ended(again);
    }
  }
  const now = (await call('GET', `/api/apps/${enc(app)}`).catch(() => ({}))).running;
  return { request: id, status: 'waiting', note: `still building after ${timeoutS} s; call app_wait again`, running: now ? { text: now.text, last: (now.lines || []).slice(-3) } : null };
}

export async function callTool(call, name, a = {}, opts = {}) {
  switch (name) {
    case 'app_list': {
      const { apps } = await call('GET', '/api/apps');
      return { apps: (apps || []).map((x) => ({ id: x.id, name: x.name, head: x.head, created: x.created, cost: x.cost })) };
    }
    case 'app_create': {
      const description = text(a.description, 'description');
      return call('POST', '/api/apps', { description, ...(a.name ? { name: String(a.name) } : {}), ...(provider(a.provider) ? { provider: a.provider } : {}) });
    }
    case 'app_state': return trimState(await call('GET', `/api/apps/${appId(a.app)}`));
    case 'app_request': {
      const id = appId(a.app);
      return call('POST', `/api/apps/${id}/requests`, { text: text(a.text, 'text'), ...(provider(a.provider) ? { provider: a.provider } : {}) });
    }
    case 'app_wait': {
      const id = appId(a.app);
      if (a.request !== undefined && !(typeof a.request === 'string' && /^[A-Za-z0-9_-]{1,40}$/.test(a.request))) bad('request is a request id');
      const t = a.timeout_s === undefined ? 600 : Number(a.timeout_s);
      if (!Number.isFinite(t) || t < 1 || t > 1800) bad('timeout_s is 1 to 1800');
      return waitFor(call, id, a.request, t, opts);
    }
    case 'piece_source': return call('GET', `/api/apps/${appId(a.app)}/source`);
    case 'screen_edit': {
      const id = appId(a.app);
      if (typeof a.slug !== 'string' || !SLUG.test(a.slug)) bad('slug is a screen slug (lowercase letters, digits, dashes)');
      return call('POST', `/api/apps/${id}/screens/${a.slug}`, { statement: text(a.statement, 'statement'), ...(a.note ? { note: String(a.note) } : {}) });
    }
    case 'data_query': {
      const id = appId(a.app);
      const no = readOnlySql(a.sql);
      if (no) bad(no);
      return call('POST', `/api/apps/${id}/sql`, { sql: a.sql });
    }
    case 'version_list': {
      const s = await call('GET', `/api/apps/${appId(a.app)}`);
      return { live: s.version?.live ?? s.app?.head, versions: (s.versions || []).map((v) => ({ n: v.n, summary: v.summary, request: v.request, created: v.created })) };
    }
    case 'version_restore': {
      const id = appId(a.app);
      if (!Number.isInteger(a.n) || a.n < 1) bad('n is a version number (from version_list)');
      return call('POST', `/api/apps/${id}/restore`, { n: a.n });
    }
    case 'library_search': return call('GET', `/api/library?q=${enc(text(a.q, 'q'))}`);
    case 'publish': {
      const id = appId(a.app);
      if (!ACCESS.includes(a.access)) bad(`access is one of ${ACCESS.join(', ')}`);
      // Publishing opens the app to the internet: never without confirm, at
      // any access (audit 4, X-G7 and X-G9). Unpublishing is always allowed.
      if (a.on !== false && a.confirm !== true) {
        bad(a.access === 'change'
          ? 'access "change" puts the app on the internet and lets any visitor change or remove any row: call again with confirm: true only if the person you work for asked for that'
          : `publishing puts the app on the internet (access "${a.access}"): call again with confirm: true only if the person you work for asked for that`);
      }
      return call('POST', `/api/apps/${id}/publish`, { on: a.on !== false, access: a.access });
    }
    default: bad(`no tool ${name}`);
  }
  return null;
}

// One JSON-RPC message in, its answer out (null for a notification).
export function server({ call, log = (m) => process.stderr.write(`${m}\n`), waitOpts } = {}) {
  const reply = (id, result) => ({ jsonrpc: '2.0', id, result });
  const error = (id, code, message) => ({ jsonrpc: '2.0', id: id ?? null, error: { code, message } });
  return async function handle(msg) {
    if (!msg || typeof msg !== 'object' || msg.jsonrpc !== '2.0' || typeof msg.method !== 'string') {
      return msg && typeof msg === 'object' && ('result' in msg || 'error' in msg) ? null : error(msg?.id, -32600, 'invalid request');
    }
    const notification = !('id' in msg);
    const { id, method, params = {} } = msg;
    try {
      switch (method) {
        case 'initialize': {
          const asked = params.protocolVersion;
          return reply(id, { protocolVersion: SUPPORTED_VERSIONS.includes(asked) ? asked : PROTOCOL_VERSION, capabilities: { tools: { listChanged: false } }, serverInfo: SERVER_INFO,
            instructions: 'Build and change ChloeDB apps. Most changes: app_request then app_wait. Small exact screen edits: piece_source then screen_edit. Data: data_query (read only).' });
        }
        case 'notifications/initialized':
        case 'notifications/cancelled':
          return null;
        case 'ping': return reply(id, {});
        case 'tools/list': return reply(id, { tools: TOOLS });
        case 'tools/call': {
          const name = params.name;
          if (!TOOLS.some((t) => t.name === name)) return error(id, -32602, `unknown tool: ${name}`);
          try {
            const out = await callTool(call, name, params.arguments || {}, waitOpts);
            return reply(id, { content: [{ type: 'text', text: JSON.stringify(out, null, 1) }], structuredContent: out && typeof out === 'object' && !Array.isArray(out) ? out : { value: out }, isError: false });
          } catch (e) {
            if (!(e instanceof ToolError)) log(`tool ${name}: ${e.stack || e.message}`);
            return reply(id, { content: [{ type: 'text', text: e.message }], isError: true });
          }
        }
        default:
          return notification ? null : error(id, -32601, `method not found: ${method}`);
      }
    } catch (e) {
      log(`${method}: ${e.stack || e.message}`);
      return notification ? null : error(id, -32603, e.message);
    }
  };
}

// stdio: one JSON message per line in, one per line out.
export function serveStdio({ url, token, input = process.stdin, output = process.stdout, log } = {}) {
  const handle = server({ call: client(url, { token }), log });
  const rl = readline.createInterface({ input, crlfDelay: Infinity });
  const write = (m) => { if (m) output.write(`${JSON.stringify(m)}\n`); };
  let open = 0;
  let closed = false;
  const maybeExit = () => { if (closed && !open) process.exit(0); };
  rl.on('line', (line) => {
    if (!line.trim()) return;
    let msg;
    try { msg = JSON.parse(line); } catch { write({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } }); return; }
    const batch = Array.isArray(msg);
    open++;
    Promise.all((batch ? msg : [msg]).map((m) => handle(m)))
      .then((outs) => { const list = outs.filter(Boolean); if (batch) { if (list.length) write(list); } else write(list[0]); })
      .finally(() => { open--; maybeExit(); });
  });
  rl.on('close', () => { closed = true; maybeExit(); });
  return rl;
}

function argOf(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  if (i >= 0 && process.argv[i + 1]) return process.argv[i + 1];
  const eq = process.argv.find((a) => a.startsWith(`--${name}=`));
  return eq ? eq.slice(name.length + 3) : fallback;
}

// The agent token: STRATA_APPS_AGENT_TOKEN, or the file named, or the
// engine's DATA/agent.token. null when there is none.
export function agentToken({ env = process.env, file } = {}) {
  if (env.STRATA_APPS_AGENT_TOKEN && env.STRATA_APPS_AGENT_TOKEN.trim()) return { token: env.STRATA_APPS_AGENT_TOKEN.trim(), from: 'STRATA_APPS_AGENT_TOKEN' };
  const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
  const f = path.resolve(file || path.join(env.STRATA_APPS_DATA || path.join(root, 'data'), 'agent.token'));
  try { const t = readFileSync(f, 'utf8').trim(); if (t) return { token: t, from: f }; } catch { /* none */ }
  return { token: null, from: f };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const url = argOf('url', process.env.STRATA_APPS_URL || 'http://127.0.0.1:7730');
  try { new URL(url); } catch { process.stderr.write(`--url ${url} is not a URL\n`); process.exit(2); }
  const { token, from } = agentToken({ file: argOf('token-file', undefined) });
  if (!token) {
    process.stderr.write(`chloedb-apps MCP server: no agent token. Start the engine once (it writes ${from}), point --token-file at that file, or set STRATA_APPS_AGENT_TOKEN to its contents.\n`);
    process.exit(2);
  }
  process.stderr.write(`chloedb-apps MCP server for ${url}, signed in with the agent token from ${from}\n`);
  serveStdio({ url, token });
}
