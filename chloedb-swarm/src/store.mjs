// Where runs go. Two stores with one interface:
//
// - DirStore: a folder of JSON files (runs/<id>/run.json, sessions.json,
//   steps.jsonl, findings.json), for the command line and for tests;
// - ServerStore: tables of a chloedb-server (swarm_runs, swarm_sessions,
//   swarm_steps, swarm_findings), which the Swarm page reads, and which SQL
//   can read too ("which findings hit the analyst persona on every run?").

import { mkdir, readFile, writeFile, appendFile, readdir, rename } from 'node:fs/promises';
import { join } from 'node:path';
import { Server } from './sandbox.mjs';

const slim = (s) => ({ ...s, persona: { ...s.persona }, task: { id: s.task.id, title: s.task.title, surface: s.task.surface, prompt: s.task.prompt } });

async function writeJson(file, v) {
  await writeFile(`${file}.tmp`, JSON.stringify(v, null, 1));
  await rename(`${file}.tmp`, file);
}
async function readJson(file, d) {
  try { return JSON.parse(await readFile(file, 'utf8')); } catch { return d; }
}

export class DirStore {
  constructor(dir) { this.dir = dir; this.sessionsOf = new Map(); this.queue = Promise.resolve(); }

  // Writes to one run's files go one after another.
  serial(fn) { this.queue = this.queue.then(fn, fn); return this.queue; }

  runDir(id) { return join(this.dir, 'runs', id); }

  async saveRun(run) {
    await mkdir(this.runDir(run.id), { recursive: true });
    return this.serial(() => writeJson(join(this.runDir(run.id), 'run.json'), run));
  }

  async saveSession(s) {
    const m = this.sessionsOf.get(s.run) || new Map();
    this.sessionsOf.set(s.run, m);
    m.set(s.id, slim(s));
    return this.serial(() => writeJson(join(this.runDir(s.run), 'sessions.json'), [...m.values()]));
  }

  async saveStep(st) { return this.serial(() => appendFile(join(this.runDir(st.run), 'steps.jsonl'), `${JSON.stringify(st)}\n`)); }

  async saveFindings(runId, findings) { return this.serial(() => writeJson(join(this.runDir(runId), 'findings.json'), findings)); }

  async runs() {
    const ids = await readdir(join(this.dir, 'runs')).catch(() => []);
    const out = [];
    for (const id of ids) { const r = await readJson(join(this.runDir(id), 'run.json'), null); if (r) out.push(r); }
    return out.sort((a, b) => b.started.localeCompare(a.started));
  }

  async run(id) {
    const run = await readJson(join(this.runDir(id), 'run.json'), null);
    if (!run) return null;
    return { run, sessions: await readJson(join(this.runDir(id), 'sessions.json'), []), findings: await readJson(join(this.runDir(id), 'findings.json'), []) };
  }

  async steps(sessionId) {
    const runId = sessionId.replace(/-s\d+$/, '');
    const text = await readFile(join(this.runDir(runId), 'steps.jsonl'), 'utf8').catch(() => '');
    return text.split('\n').filter(Boolean).map((l) => JSON.parse(l)).filter((s) => s.session === sessionId).sort((a, b) => a.step - b.step);
  }
}

// The tables: each row's whole record is one JSON field (`doc`), with the
// columns a query filters or sorts on beside it.
export const TABLES = {
  swarm_runs: 'CREATE TABLE IF NOT EXISTS swarm_runs (id KEYWORD PRIMARY KEY, name KEYWORD, status KEYWORD, started TIMESTAMP, usd FLOAT, rate FLOAT, doc JSON)',
  swarm_sessions: 'CREATE TABLE IF NOT EXISTS swarm_sessions (id KEYWORD PRIMARY KEY, run KEYWORD, persona KEYWORD, task KEYWORD, model KEYWORD, outcome KEYWORD, success BOOL, steps INT, usd FLOAT, answer TEXT, doc JSON)',
  swarm_steps: 'CREATE TABLE IF NOT EXISTS swarm_steps (id KEYWORD PRIMARY KEY, run KEYWORD, session KEYWORD, step INT, tool KEYWORD, ok BOOL, thought TEXT, doc JSON)',
  swarm_findings: 'CREATE TABLE IF NOT EXISTS swarm_findings (id KEYWORD PRIMARY KEY, run KEYWORD, rank INT, category KEYWORD, severity INT, share FLOAT, title TEXT, summary TEXT, doc JSON)',
};

// A JSON field takes an object; arrays and long strings stay inside it.
const docOf = (v) => JSON.parse(JSON.stringify(v));

export class ServerStore {
  constructor(url, token) { this.server = new Server(url, token, { 'x-chloedb-app': 'swarm' }); this.ready = null; }

  async init() {
    this.ready ||= (async () => { for (const sql of Object.values(TABLES)) await this.server.sql(sql); })();
    return this.ready;
  }

  async put(table, row) { await this.init(); return this.server.import(table, [row]); }

  saveRun(r) { return this.put('swarm_runs', { id: r.id, name: r.name, status: r.status, started: r.started, usd: r.cost?.usd || 0, rate: r.totals?.rate ?? null, doc: docOf(r) }); }

  saveSession(s) {
    const d = slim(s);
    return this.put('swarm_sessions', { id: s.id, run: s.run, persona: s.persona.id, task: s.task.id, model: s.model, outcome: s.outcome, success: !!s.success, steps: s.steps || 0, usd: s.cost || 0, answer: s.answer || '', doc: docOf(d) });
  }

  saveStep(st) { return this.put('swarm_steps', { id: st.id, run: st.run, session: st.session, step: st.step, tool: st.tool, ok: !!st.ok, thought: st.thought || '', doc: docOf(st) }); }

  async saveFindings(runId, findings) {
    await this.init();
    // A run's findings are replaced as a whole, so a regroup leaves none behind.
    await this.server.sql('DELETE FROM swarm_findings WHERE run = :id', { id: runId });
    if (!findings.length) return null;
    return this.server.import('swarm_findings', findings.map((f) => ({ id: f.id, run: runId, rank: f.rank, category: f.category, severity: f.severity, share: f.share, title: f.title, summary: f.summary, doc: docOf(f) })));
  }

  async docs(sql, params) {
    await this.init();
    const rows = await this.server.sql(sql, params);
    return rows.map((r) => (typeof r.doc === 'string' ? JSON.parse(r.doc) : r.doc));
  }

  runs() { return this.docs('SELECT doc FROM swarm_runs ORDER BY started DESC LIMIT 200'); }

  async run(id) {
    const [run] = await this.docs('SELECT doc FROM swarm_runs WHERE id = :id', { id });
    if (!run) return null;
    const sessions = await this.docs('SELECT doc FROM swarm_sessions WHERE run = :id ORDER BY id LIMIT 5000', { id });
    const findings = await this.docs('SELECT doc FROM swarm_findings WHERE run = :id ORDER BY rank LIMIT 500', { id });
    return { run, sessions, findings };
  }

  steps(sessionId) { return this.docs('SELECT doc FROM swarm_steps WHERE session = :id ORDER BY step LIMIT 1000', { id: sessionId }); }
}
