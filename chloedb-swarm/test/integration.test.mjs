// A whole run against real chloedb-servers, with a scripted model in place
// of the API: each session gets a sandbox, the agent acts through the MCP
// surface, the checks read the sandbox, the judge's answers are grouped into
// findings, and the run lands in both stores. Skipped when no chloedb-server
// is built (CHLOEDB_SERVER_BIN, or chloedb-server/target/{release,ci,debug}).

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { serverBin, Sandbox } from '../src/sandbox.mjs';
import { useClient } from '../src/model.mjs';
import { runSet, regroup } from '../src/runner.mjs';
import { DirStore, ServerStore } from '../src/store.mjs';
import { scripted } from './helpers.mjs';

let bin = null;
try { bin = serverBin(); } catch { /* skipped below */ }
const skip = bin ? false : 'no chloedb-server binary built';
// CI sets CHLOEDB_SWARM_NEED_SERVER, so that a missing build fails rather than skips.
if (!bin && process.env.CHLOEDB_SWARM_NEED_SERVER) throw new Error('CHLOEDB_SWARM_NEED_SERVER is set but no chloedb-server was found');

const set = {
  name: 'it', seeds: 2, models: ['haiku'], budget_usd: 5, concurrency: 2,
  personas: [{ id: 'dev', name: 'Dev One', role: 'a developer', traits: ['Quick.'] }],
  tasks: [{
    id: 'count', title: 'Count the tickets', surface: 'mcp', scope: 'read',
    prompt: 'How many tickets are there?',
    fixture: { tables: [{ name: 'tickets', create: 'CREATE TABLE tickets (id KEYWORD PRIMARY KEY, subject TEXT)', rows: [{ id: 't-1', subject: 'refund' }, { id: 't-2', subject: 'login' }, { id: 't-3', subject: 'refund again' }] }] },
    checks: [
      { name: 'loaded', kind: 'sql', sql: 'SELECT COUNT(*) AS n FROM tickets', expect: { n: 3 } },
      { name: 'answer', kind: 'answer_matches_sql', sql: 'SELECT COUNT(*) AS n FROM tickets', columns: ['n'] },
    ],
  }],
};

// The model's part, chosen by what each request offers: the judge's review
// and grouping, the exit interview, or the next step of the session.
function pick(params) {
  const tools = params.tools.map((t) => t.name);
  if (tools.includes('review')) {
    return { tool: 'review', input: { summary: 'Counted the tickets.', verdict: 'mixed', problems: [{ step: 1, category: 'docs', severity: 1, title: 'No row count in list_tables', detail: 'The person had to write SQL to count.' }], strengths: [{ step: 2, title: 'COUNT answered in one step', detail: 'The query tool gave the count at once.' }] } };
  }
  if (tools.includes('findings')) {
    const ids = [...params.messages[0].content.matchAll(/\[([^\]]+#\d+)\]/g)].map((m) => m[1]);
    return { tool: 'findings', input: { findings: [{ title: 'No row counts', category: 'docs', severity: 1, summary: 'Counting needs SQL.', fix: 'Show counts.', problems: ids }] } };
  }
  if (tools.includes('strengths')) {
    const good = [...params.messages[0].content.matchAll(/\[([^\]]+\+\d+)\]/g)].map((m) => m[1]);
    return { tool: 'strengths', input: { strengths: [{ title: 'Counts in one step', summary: 'SQL counts work at once.', problems: good }] } };
  }
  if (tools.includes('interview')) return { tool: 'interview', input: { ease: 6, hardest: 'none', expected: 'a count', one_change: 'counts', would_use: 'yes' } };
  const turn = params.messages.filter((m) => m.role === 'assistant').length;
  if (turn === 0) return { tool: 'list_tables', input: { note: 'What is here?' } };
  if (turn === 1) return { tool: 'query', input: { note: 'Count them.', sql: 'SELECT COUNT(*) AS n FROM tickets' } };
  return { tool: 'finish', input: { note: 'Got it.', outcome: 'done', answer: 'There are 3 tickets.' } };
}

test('a run plays each session in its own sandbox and lands in both stores', { skip, timeout: 120_000 }, async () => {
  const work = await mkdtemp(join(tmpdir(), 'swarm-it-'));
  const home = await Sandbox.start({ bin, dir: join(work, 'home') });
  try {
    for (const [name, store] of [['dir', new DirStore(join(work, 'out'))], ['server', new ServerStore(home.url, home.token)]]) {
      const model = scripted([], pick);
      useClient(model);
      const run = await runSet(set, { store, workdir: join(work, 'sb'), bin });
      assert.equal(run.status, 'done', name);
      assert.equal(run.totals.sessions, 2);
      assert.equal(run.totals.ok, 2, JSON.stringify(run.totals));
      assert.equal(run.totals.findings, 1);
      assert.equal(run.totals.strengths, 1);
      const got = await store.run(run.id);
      assert.equal(got.sessions.length, 2);
      for (const s of got.sessions) {
        assert.equal(s.outcome, 'done');
        assert.deepEqual(s.checks.map((c) => c.ok), [true, true]);
        assert.equal(s.interview.ease, 6);
        assert.equal(s.review.problems.length, 1);
        const steps = await store.steps(s.id);
        assert.deepEqual(steps.map((st) => st.tool), ['list_tables', 'query', 'finish']);
        assert.ok(steps[1].ok && /3/.test(steps[1].result), steps[1].result);
      }
      assert.equal(got.findings[0].sessions.length, 2);
      assert.equal(got.findings[0].share, 1);
      assert.deepEqual(got.findings.map((f) => f.category), ['docs', 'strength']);
      assert.equal(got.findings[1].sessions.length, 2);
      // Grouped again from the kept reviews, the findings are replaced, not added to.
      const again = await regroup(store, run.id);
      assert.equal(again.totals.findings, 1);
      assert.equal((await store.run(run.id)).findings.length, 2);
      // Every session's requests carried the swarm's tool list and spoke to its own sandbox.
      assert.ok(model.requests.some((p) => p.tools.some((t) => t.name === 'describe_table')));
      // The sandboxes are gone once the run ends.
      const left = await import('node:fs/promises').then((f) => f.readdir(join(work, 'sb', run.id)).catch(() => []));
      assert.deepEqual(left, []);
    }
  } finally {
    await home.stop();
    await rm(work, { recursive: true, force: true });
  }
});

let hasPlaywright = true;
try { (await import('../src/surfaces/browser.mjs')).playwright(); } catch { hasPlaywright = false; }

test('browser: the command center as an accessibility tree, acted on by reference', { skip: skip || (!hasPlaywright && 'no Playwright'), timeout: 120_000 }, async () => {
  const { BrowserSurface, closeBrowser } = await import('../src/surfaces/browser.mjs');
  const work = await mkdtemp(join(tmpdir(), 'swarm-br-'));
  const sb = await Sandbox.start({ bin, dir: join(work, 'sb') });
  const key = await sb.key('t', 'admin');
  const s = await new BrowserSurface({ url: sb.url, key }).open();
  try {
    const first = await s.start();
    assert.match(first, /Page: .*ChloeDB/);
    assert.ok(!first.includes(key), 'the key stays out of what the agent reads');
    const link = first.match(/link "Queries" \[ref=(e\d+)\]/);
    assert.ok(link, first.slice(0, 800));
    const r = await s.call('click', { note: 'Open queries.', ref: link[1] });
    assert.ok(r.ok, r.error);
    assert.match(r.url, /#\/queries/);
    const bad = await s.call('click', { note: 'x', ref: 'e99999' });
    assert.equal(bad.ok, false);
    assert.match(bad.error, /no element has that reference/);
    const away = await s.call('go', { note: 'x', path: 'https://example.com/' });
    assert.match(away.error, /only pages of this site/);
  } finally {
    await s.close();
    await closeBrowser();
    await sb.stop();
    await rm(work, { recursive: true, force: true });
  }
});
