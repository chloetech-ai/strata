// A run: every session of a scenario set (each persona on each task, for
// each seed), a few at a time, each in a sandbox of its own; then a review
// of each session and the findings of the whole run. Everything goes to a
// store as it happens, so the command center shows a run while it plays.

import { rm, mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { randomBytes } from 'node:crypto';
import { Sandbox, serverBin } from './sandbox.mjs';
import { McpSurface } from './surfaces/mcp.mjs';
import { BrowserSurface, closeBrowser } from './surfaces/browser.mjs';
import { runSession } from './agent.mjs';
import { runChecks } from './checks.mjs';
import { reviewSession, groupFindings } from './judge.mjs';
import { Spend, modelId } from './model.mjs';

export const newId = (p) => `${p}${new Date().toISOString().slice(0, 10).replace(/-/g, '')}-${randomBytes(3).toString('hex')}`;

/** The sessions of a set: [{ persona, task, seed, model }]. */
export function expand(set, { only = {}, limit = 0 } = {}) {
  const seeds = set.seeds || 1;
  const models = (set.models?.length ? set.models : ['haiku']).map(modelId);
  const jobs = [];
  for (const task of set.tasks) {
    if (only.task && !only.task.includes(task.id)) continue;
    for (const persona of set.personas) {
      if (only.persona && !only.persona.includes(persona.id)) continue;
      if (task.personas && !task.personas.includes(persona.id)) continue;
      for (let seed = 1; seed <= seeds; seed++) {
        if (only.seed && !only.seed.includes(seed)) continue;
        jobs.push({ persona, task, seed, model: models[(seed - 1) % models.length] });
      }
    }
  }
  return limit ? jobs.slice(0, limit) : jobs;
}

/** Share of sessions that succeeded, overall and by a key. */
export function rates(sessions, key) {
  const by = {};
  for (const s of sessions) {
    const k = key ? key(s) : 'all';
    const b = (by[k] ||= { n: 0, ok: 0, steps: 0, ease: 0, eased: 0 });
    b.n++;
    if (s.success) b.ok++;
    b.steps += s.steps || 0;
    if (s.interview?.ease) { b.ease += s.interview.ease; b.eased++; }
  }
  for (const b of Object.values(by)) {
    b.rate = b.n ? Number((b.ok / b.n).toFixed(3)) : 0;
    b.steps = b.n ? Number((b.steps / b.n).toFixed(1)) : 0;
    b.ease = b.eased ? Number((b.ease / b.eased).toFixed(1)) : null;
    delete b.eased;
  }
  return key ? by : by.all;
}

async function pool(items, n, fn, stopped) {
  let i = 0;
  const workers = Array.from({ length: Math.max(1, n) }, async () => {
    while (i < items.length && !stopped()) { const j = i++; await fn(items[j], j); }
  });
  await Promise.all(workers);
}

/** The problems and strengths of the sessions' reviews, each with an id. */
export function reviewed(sessions) {
  const problems = [];
  const strengths = [];
  for (const s of sessions) {
    (s.review?.problems || []).forEach((p, i) => problems.push({ id: `${s.id}#${i + 1}`, session: s.id, persona: s.persona.id, task: s.task.id, ...p }));
    (s.review?.strengths || []).forEach((p, i) => strengths.push({ id: `${s.id}+${i + 1}`, session: s.id, persona: s.persona.id, task: s.task.id, ...p }));
  }
  return { problems, strengths };
}

const findingTotals = (problems, findings) => ({
  problems: problems.length,
  findings: findings.filter((f) => f.category !== 'strength').length,
  strengths: findings.filter((f) => f.category === 'strength').length,
  agent_mistakes: problems.filter((p) => p.category === 'agent_mistake').length,
});

async function groupAndSave(store, runId, sessions, judge, log) {
  const { problems, strengths } = reviewed(sessions);
  let findings = [];
  try { findings = (await groupFindings(judge, problems, sessions, { strengths })).findings; } catch (e) { log(`grouping the findings failed: ${e.message}`); }
  await store.saveFindings(runId, findings.map((f) => ({ id: `${runId}-f${String(f.rank).padStart(2, '0')}`, run: runId, ...f })));
  return { problems, findings };
}

/**
 * Group a finished run's findings again from the reviews it kept, without
 * playing or reviewing any session: after a grouping that failed, or with
 * a newer judge. Its findings are replaced; the run's totals and judge cost
 * follow.
 */
export async function regroup(store, runId, { budget = 5, log = () => {} } = {}) {
  const all = await store.run(runId);
  if (!all) throw new Error(`no run ${runId}`);
  const { run, sessions } = all;
  const judge = new Spend(budget);
  const { problems, findings } = await groupAndSave(store, runId, sessions, judge, log);
  run.totals = { ...(run.totals || {}), ...findingTotals(problems, findings) };
  const j = run.cost?.judge || { usd: 0, calls: 0 };
  run.cost = { ...run.cost, judge: { ...j, usd: Number(((j.usd || 0) + judge.usd).toFixed(4)), calls: (j.calls || 0) + judge.calls } };
  run.cost.usd = Number(((run.cost.users?.usd || 0) + run.cost.judge.usd).toFixed(4));
  await store.saveRun(run);
  log(`run ${runId}: ${run.totals.findings} findings and ${run.totals.strengths} strengths from ${problems.length} problems, $${judge.usd.toFixed(4)}`);
  return run;
}

/**
 * Play a scenario set. `store` gets the run, its sessions, steps and
 * findings; `workdir` holds the sandboxes. Answers the finished run.
 */
export async function runSet(set, { store, workdir, bin = serverBin(), budget = set.budget_usd || 25, concurrency = set.concurrency || 4, only, limit, keep = false, log = () => {}, control = {} } = {}) {
  const jobs = expand(set, { only, limit });
  const users = new Spend(budget * 0.75);
  const judge = new Spend(budget * 0.25);
  const run = {
    id: newId('run-'), name: set.name, description: set.description || '', status: 'running',
    started: new Date().toISOString(), ended: null, planned: jobs.length, done: 0,
    set: { name: set.name, seeds: set.seeds || 1, models: set.models || ['haiku'], personas: set.personas.map((p) => p.id), tasks: set.tasks.map((t) => t.id) },
    totals: null, cost: { users: users.toJSON(), judge: judge.toJSON(), usd: 0 },
  };
  control.run = run;
  await store.saveRun(run);
  log(`run ${run.id}: ${jobs.length} sessions, budget $${budget}`);
  const sessions = [];
  const stopped = () => control.stop || users.over;

  await pool(jobs, concurrency, async (job, j) => {
    const sid = `${run.id}-s${String(j + 1).padStart(3, '0')}`;
    const s = {
      id: sid, run: run.id, persona: job.persona, task: job.task, seed: job.seed, model: job.model,
      surface: job.task.surface, outcome: 'running', success: false, steps: 0, seconds: 0, cost: 0,
      answer: '', checks: [], interview: null, review: null, errors: { http4xx: 0, http5xx: 0, console: 0, failed_steps: 0 }, error: null,
      started: new Date().toISOString(),
    };
    await store.saveSession(s);
    const dir = join(workdir, run.id, sid);
    let sb = null;
    let surface = null;
    try {
      sb = await Sandbox.start({ bin, dir });
      await sb.load(job.task.fixture, job.seed);
      const key = await sb.key(`swarm-${job.persona.id}`, job.task.scope || 'write');
      const headers = { 'x-chloedb-app': 'swarm', 'x-chloedb-swarm': `${run.id}/${sid}` };
      surface = job.task.surface === 'browser'
        ? new BrowserSurface({ url: sb.url, key, headers, entry: job.task.entry, device: job.persona.device })
        : new McpSurface({ url: sb.url, key, headers });
      await surface.open();
      const r = await runSession({
        persona: job.persona, task: job.task, surface, model: job.model, spend: users,
        maxSteps: job.task.max_steps || 25, maxSeconds: job.task.max_seconds || 600,
        onStep: async (st) => {
          s.steps = st.step;
          if (!st.ok) s.errors.failed_steps++;
          for (const e of st.events || []) {
            if (e.kind === 'console' || e.kind === 'exception') s.errors.console++;
            else if (e.kind === 'http' && /^5/.test(e.text)) s.errors.http5xx++;
            else if (e.kind === 'http') s.errors.http4xx++;
          }
          if (st.http >= 500) s.errors.http5xx++;
          await store.saveStep({ id: `${sid}-${String(st.step).padStart(3, '0')}`, run: run.id, session: sid, ts: new Date().toISOString(), ...st });
        },
      });
      Object.assign(s, { outcome: r.outcome, answer: r.answer, interview: r.interview, steps: r.steps, seconds: r.seconds, cost: r.cost, error: r.error });
      if (surface.screenshot) {
        await mkdir(join(workdir, run.id, 'screens'), { recursive: true });
        s.screenshot = await surface.screenshot(join(workdir, run.id, 'screens', `${sid}.png`));
      }
      const c = await runChecks(job.task, { sandbox: sb, outcome: r.outcome, answer: r.answer });
      s.success = c.success;
      s.checks = c.results;
    } catch (e) {
      s.outcome = s.outcome === 'running' ? 'error' : s.outcome;
      s.error = e.message;
    } finally {
      await surface?.close().catch(() => {});
      await sb?.stop().catch(() => {});
      if (!keep) await rm(dir, { recursive: true, force: true }).catch(() => {});
    }
    s.ended = new Date().toISOString();
    sessions.push(s);
    run.done++;
    run.cost = { users: users.toJSON(), judge: judge.toJSON(), usd: Number((users.usd + judge.usd).toFixed(4)) };
    await store.saveSession(s);
    await store.saveRun(run);
    log(`${sid} ${job.persona.id} / ${job.task.id} / seed ${job.seed} (${job.model}): ${s.outcome}${s.success ? ' ✓' : ''}, ${s.steps} steps, $${s.cost.toFixed(4)}${s.error ? ` — ${s.error.slice(0, 120)}` : ''}`);
  }, stopped);
  await closeBrowser();

  // Review each session, then group what the reviews found.
  run.status = 'judging';
  await store.saveRun(run);
  await pool(sessions, concurrency, async (s) => {
    if (s.outcome === 'error' && !s.steps) return;
    try {
      const steps = await store.steps(s.id);
      s.review = await reviewSession(judge, s, steps);
      await store.saveSession(s);
    } catch (e) { log(`review of ${s.id} failed: ${e.message}`); }
  }, () => judge.over);
  const { problems, findings } = await groupAndSave(store, run.id, sessions, judge, log);

  run.status = control.stop ? 'stopped' : users.over ? 'budget' : 'done';
  run.ended = new Date().toISOString();
  run.totals = {
    sessions: sessions.length, ...rates(sessions),
    by_persona: rates(sessions, (s) => s.persona.id), by_task: rates(sessions, (s) => s.task.id), by_model: rates(sessions, (s) => s.model),
    outcomes: sessions.reduce((m, s) => ({ ...m, [s.outcome]: (m[s.outcome] || 0) + 1 }), {}),
    ...findingTotals(problems, findings),
  };
  run.cost = { users: users.toJSON(), judge: judge.toJSON(), usd: Number((users.usd + judge.usd).toFixed(4)) };
  await store.saveRun(run);
  log(`run ${run.id} ${run.status}: ${Math.round(run.totals.rate * 100)}% of ${sessions.length} sessions succeeded, ${findings.length} findings, $${run.cost.usd}`);
  return run;
}
