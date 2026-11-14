#!/usr/bin/env node
// chloedb-swarm: simulated users for ChloeDB.
//
//   chloedb-swarm run scenarios/starter.json [--out DIR | --server URL --token T]
//                 [--budget USD] [--concurrency N] [--limit N]
//                 [--persona a,b] [--task x,y] [--seed 1,2] [--keep]
//   chloedb-swarm serve --local DIR [--listen 127.0.0.1:7900]
//   chloedb-swarm serve --server URL [--token T] [--listen 127.0.0.1:7900]
//   chloedb-swarm regroup RUN_ID [--out DIR | --server URL --token T] [--budget USD]
//   chloedb-swarm report DIR RUN_ID
//
// The model key is ANTHROPIC_API_KEY (or CHLOEDB_SWARM_ANTHROPIC_KEY); the
// sandboxes run CHLOEDB_SERVER_BIN, or the repository's chloedb-server build.

import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import { runSet, regroup } from '../src/runner.mjs';
import { DirStore, ServerStore } from '../src/store.mjs';
import { markdown } from '../src/report.mjs';
import { serve } from '../src/serve.mjs';

function flags(argv) {
  const out = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('--')) { out._.push(a); continue; }
    const k = a.slice(2);
    const v = argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[++i] : true;
    out[k] = v;
  }
  return out;
}
const list = (v) => (typeof v === 'string' ? v.split(',').map((s) => s.trim()).filter(Boolean) : undefined);

async function main() {
  const [cmd, ...rest] = process.argv.slice(2);
  const f = flags(rest);
  if (cmd === 'run') {
    const file = f._[0];
    if (!file) throw new Error('chloedb-swarm run SET.json');
    const set = JSON.parse(await readFile(file, 'utf8'));
    const out = resolve(f.out || 'swarm-runs');
    const store = f.server ? new ServerStore(f.server, f.token === true ? undefined : f.token) : new DirStore(out);
    await mkdir(out, { recursive: true });
    const only = { persona: list(f.persona), task: list(f.task), seed: list(f.seed)?.map(Number) };
    const control = {};
    process.on('SIGINT', () => { if (control.stop) process.exit(130); control.stop = true; console.error('stopping after the sessions playing now (Ctrl-C again to quit)'); });
    const run = await runSet(set, {
      store, workdir: join(out, 'sandboxes'), budget: f.budget ? Number(f.budget) : undefined,
      concurrency: f.concurrency ? Number(f.concurrency) : undefined, limit: f.limit ? Number(f.limit) : 0,
      only, keep: !!f.keep, control, log: (m) => console.log(m),
    });
    const all = await store.run(run.id);
    if (store instanceof DirStore) {
      const md = join(store.runDir(run.id), 'report.md');
      await writeFile(md, markdown(all));
      console.log(`report: ${md}`);
    }
    return;
  }
  if (cmd === 'serve') {
    await serve({ local: f.local === true ? undefined : f.local, server: f.server, token: f.token === true ? undefined : f.token, listen: f.listen || '127.0.0.1:7900', scenarios: f.scenarios });
    return;
  }
  if (cmd === 'regroup') {
    const id = f._[0];
    if (!id) throw new Error('chloedb-swarm regroup RUN_ID [--out DIR | --server URL --token T]');
    const store = f.server ? new ServerStore(f.server, f.token === true ? undefined : f.token) : new DirStore(resolve(f.out || 'swarm-runs'));
    await regroup(store, id, { budget: f.budget ? Number(f.budget) : 5, log: (m) => console.log(m) });
    if (store instanceof DirStore) await writeFile(join(store.runDir(id), 'report.md'), markdown(await store.run(id)));
    return;
  }
  if (cmd === 'report') {
    const [dir, id] = f._;
    const all = await new DirStore(resolve(dir)).run(id);
    if (!all) throw new Error(`no run ${id} in ${dir}`);
    process.stdout.write(markdown(all));
    return;
  }
  console.log('chloedb-swarm run SET.json | serve | regroup RUN_ID | report DIR RUN_ID (see the head of bin/swarm.mjs)');
  process.exitCode = cmd ? 2 : 0;
}

main().catch((e) => { console.error(`chloedb-swarm: ${e.message}`); process.exit(1); });
