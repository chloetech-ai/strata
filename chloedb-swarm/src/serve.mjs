// The swarm as an app of a ChloeDB server: the command center's Swarm page
// (#/app/swarm) lists the scenario sets, starts a run, and shows runs,
// findings and every session step by step. chloedb-server forwards
// /v1/apps/swarm/<path> here with CHLOEDB_APP_TOKEN, after checking the
// caller's key (admin for anything but GET), so this server takes no
// requests from browsers and needs no keys of its own.
//
//   serve --local DIR       starts a chloedb-server on DIR with this app
//                           listed, and keeps the runs in its tables
//   serve --server URL      runs beside an existing server (which lists it
//                           with --app swarm=http://this-address)

import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { readFile, readdir, mkdir, open } from 'node:fs/promises';
import { join, resolve, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { ServerStore } from './store.mjs';
import { Server, serverBin } from './sandbox.mjs';
import { runSet } from './runner.mjs';

const HERE = fileURLToPath(new URL('.', import.meta.url));
const UI = resolve(HERE, '../ui');
const SCENARIOS = resolve(HERE, '../scenarios');

export const ICON = '<circle cx="7" cy="8" r="2.5"/><circle cx="17" cy="8" r="2.5"/><circle cx="12" cy="15" r="2.5"/><path d="M3 20c.5-2.5 2-4 4-4M21 20c-.5-2.5-2-4-4-4M8.5 21c.7-1.5 2-2.5 3.5-2.5s2.8 1 3.5 2.5"/>';

export function manifest() {
  return { name: 'swarm', title: 'Swarm', description: 'Simulated users with a persona and a task try ChloeDB in sandboxes; see what worked and what did not', icon_svg: ICON, view: 'ui/view.js', version: '0.1.0', maturity: 'demo' };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Start the home chloedb-server of `serve --local`, listing this app. */
async function startHome(dir, appUrl, appToken, listen = '127.0.0.1:7700') {
  await mkdir(dir, { recursive: true });
  const log = await open(join(dir, 'chloedb-server.log'), 'a');
  const child = spawn(serverBin(), ['--db', join(dir, 'swarm.strata'), '--listen', listen, '--grpc', 'off', '--app', `swarm=${appUrl}`], {
    stdio: ['ignore', log.fd, log.fd], env: { ...process.env, CHLOEDB_APP_TOKEN: appToken, STRATA_APP_TOKEN: appToken },
  });
  const url = `http://${listen}`;
  const s = new Server(url);
  for (let i = 0; i < 600; i++) {
    if (child.exitCode !== null) throw new Error(`chloedb-server exited: see ${join(dir, 'chloedb-server.log')}`);
    try { await s.health(); break; } catch { await sleep(100); }
  }
  child.log = log; // closed with the process, not by the collector
  const stop = () => { try { child.kill('SIGTERM'); } catch { /* gone */ } };
  process.on('exit', stop);
  for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => { stop(); process.exit(0); });
  return { url, child };
}

async function scenarioSets(dir) {
  const out = [];
  for (const f of (await readdir(dir).catch(() => [])).filter((n) => n.endsWith('.json')).sort()) {
    try {
      const s = JSON.parse(await readFile(join(dir, f), 'utf8'));
      out.push({ file: f, name: s.name || basename(f, '.json'), description: s.description || '', seeds: s.seeds || 1, models: s.models || ['haiku'], budget_usd: s.budget_usd || 25, personas: s.personas.map(({ id, name, role, device }) => ({ id, name, role, device })), tasks: s.tasks.map(({ id, title, surface, prompt }) => ({ id, title, surface, prompt: prompt.split('\n')[0] })) });
    } catch (e) { out.push({ file: f, error: e.message }); }
  }
  return out;
}

const json = (res, status, body) => { res.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' }); res.end(JSON.stringify(body)); };

async function body(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const t = Buffer.concat(chunks).toString('utf8');
  return t ? JSON.parse(t) : {};
}

export async function serve({ local, server, token, listen = '127.0.0.1:7900', scenarios = SCENARIOS, homeListen } = {}) {
  const [host, port] = listen.split(':');
  let appToken = process.env.CHLOEDB_APP_TOKEN || process.env.STRATA_APP_TOKEN || null;
  let home = server;
  if (local) {
    appToken ||= randomBytes(24).toString('hex');
    ({ url: home } = await startHome(resolve(local), `http://${listen}`, appToken, homeListen));
  }
  if (!home) throw new Error('give --local DIR or --server URL');
  const store = new ServerStore(home, token || process.env.CHLOEDB_TOKEN);
  await store.init();
  const workdir = resolve(local || '.', 'sandboxes');
  const control = { run: null, stop: false, busy: false };

  const routes = async (req, res, url) => {
    const p = url.pathname;
    if (req.method === 'GET' && p === '/app.json') return json(res, 200, manifest());
    if (req.method === 'GET' && p === '/ui/view.js') {
      const js = await readFile(join(process.env.CHLOEDB_SWARM_UI_DIR || UI, 'view.js'), 'utf8');
      res.writeHead(200, { 'content-type': 'text/javascript; charset=utf-8', 'cache-control': 'no-cache' });
      return res.end(js);
    }
    if (req.method === 'GET' && p === '/v1/info') {
      return json(res, 200, { scenarios: await scenarioSets(scenarios), running: control.busy ? control.run : null, model_key: !!(process.env.CHLOEDB_SWARM_ANTHROPIC_KEY || process.env.ANTHROPIC_API_KEY), server_bin: serverBin() });
    }
    if (req.method === 'GET' && p === '/v1/runs') return json(res, 200, { runs: await store.runs() });
    let m = p.match(/^\/v1\/runs\/([\w-]+)$/);
    if (req.method === 'GET' && m) { const r = await store.run(m[1]); return r ? json(res, 200, r) : json(res, 404, { error: 'no such run' }); }
    m = p.match(/^\/v1\/sessions\/([\w-]+)\/steps$/);
    if (req.method === 'GET' && m) return json(res, 200, { steps: await store.steps(m[1]) });
    if (req.method === 'POST' && p === '/v1/runs') {
      if (control.busy) return json(res, 409, { error: `run ${control.run?.id} is playing; stop it or wait` });
      const b = await body(req);
      const files = (await readdir(scenarios)).filter((n) => n.endsWith('.json'));
      const file = files.find((f) => f === b.scenario || f === `${b.scenario}.json`);
      if (!file) return json(res, 404, { error: `no scenario set ${b.scenario}` });
      const set = JSON.parse(await readFile(join(scenarios, file), 'utf8'));
      Object.assign(control, { stop: false, busy: true, run: null });
      const done = runSet(set, {
        store, workdir, control, budget: b.budget ? Number(b.budget) : undefined, limit: b.limit ? Number(b.limit) : 0,
        only: { persona: b.persona, task: b.task, seed: b.seed }, log: (msg) => console.log(msg),
      }).catch((e) => console.error(`run failed: ${e.message}`)).finally(() => { control.busy = false; });
      for (let i = 0; i < 50 && !control.run; i++) await sleep(20);
      void done;
      return json(res, 202, { run: control.run });
    }
    m = p.match(/^\/v1\/runs\/([\w-]+)\/stop$/);
    if (req.method === 'POST' && m) {
      if (!control.busy || control.run?.id !== m[1]) return json(res, 409, { error: 'that run is not playing' });
      control.stop = true;
      return json(res, 202, { stopping: m[1] });
    }
    return json(res, 404, { error: 'not found' });
  };

  const http = createServer(async (req, res) => {
    try {
      if (req.headers.origin) return json(res, 403, { error: 'the swarm takes no requests from browsers: open it in ChloeDB\'s command center (chloedb-server --app swarm=URL)' });
      if (appToken) {
        const given = Buffer.from((req.headers.authorization || '').replace(/^Bearer /, ''));
        const want = Buffer.from(appToken);
        if (given.length !== want.length || !timingSafeEqual(given, want)) return json(res, 401, { error: 'this app needs its token (Authorization: Bearer ...)' });
      }
      await routes(req, res, new URL(req.url, 'http://x'));
    } catch (e) {
      json(res, 500, { error: e.message });
    }
  });
  await new Promise((ok) => http.listen(Number(port), host, ok));
  console.log(`chloedb-swarm on http://${listen}${local ? `; command center: ${home}/#/app/swarm` : ''}`);
  return { http, home, store, control };
}
