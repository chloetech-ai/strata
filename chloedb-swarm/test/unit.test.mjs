// The parts that need neither a model nor a server: fixtures, checks,
// costs, how a set expands into sessions, the agent loop against a
// scripted model and surface, and the report.

import test from 'node:test';
import assert from 'node:assert/strict';
import { rows, rng } from '../src/fixtures.mjs';
import { holds, fill, runChecks } from '../src/checks.mjs';
import { costOf, Spend, useClient, modelId } from '../src/model.mjs';
import { expand, rates } from '../src/runner.mjs';
import { runSession } from '../src/agent.mjs';
import { why, subtree, fit } from '../src/surfaces/browser.mjs';
import { markdown } from '../src/report.mjs';
import { scripted, FakeSurface, persona, task } from './helpers.mjs';

test('fixtures: the same seed draws the same rows, another seed others', () => {
  const spec = { count: 50, fields: { id: { seq: 'o-' }, status: { pick: ['a', 'b'], weights: [1, 3] }, n: { int: [1, 3] }, x: { float: [0, 1], digits: 3 }, at: { daysAgo: [0, 10] }, k: { const: 7 } } };
  const now = Date.parse('2026-11-12T00:00:00Z');
  const a = rows(spec, 1, now);
  assert.deepEqual(a, rows(spec, 1, now));
  assert.notDeepEqual(a, rows(spec, 2, now));
  assert.equal(a[0].id, 'o-1');
  assert.equal(a[49].id, 'o-50');
  for (const r of a) {
    assert.ok(['a', 'b'].includes(r.status));
    assert.ok(r.n >= 1 && r.n <= 3 && Number.isInteger(r.n));
    const age = (now - Date.parse(r.at)) / 86400000;
    assert.ok(age >= 0 && age <= 10, `age ${age}`);
    assert.equal(r.k, 7);
  }
  assert.ok(a.filter((r) => r.status === 'b').length > a.filter((r) => r.status === 'a').length);
  const r = rng(3);
  for (let i = 0; i < 1000; i++) { const x = r(); assert.ok(x >= 0 && x < 1); }
});

test('checks: an answer holds ids as words and numbers as numbers', () => {
  assert.ok(holds('Tickets t-03, t-08 and t-14.', 't-08'));
  assert.ok(!holds('Tickets t-030 only', 't-03'));
  assert.ok(!holds('ticket t-3', 't-03'));
  assert.ok(holds('delivered: 1,234 orders', 1234));
  assert.ok(holds('delivered 29, paid 17', '17'));
  assert.ok(!holds('delivered 29, paid 17', 18));
  assert.ok(holds('average 12.34', 12.3412));
  assert.ok(holds('Status: REFUNDED (6)', 'refunded'));
});

test('checks: {{days_ago:N}} and {{now}} become times', () => {
  const now = Date.parse('2026-11-12T12:00:00Z');
  assert.equal(fill("x >= '{{days_ago:7}}' AND y < '{{now}}'", now), "x >= '2026-11-05T12:00:00.000Z' AND y < '2026-11-12T12:00:00.000Z'");
});

test('checks: success needs "done" and every check passing', async () => {
  const sandbox = {
    sql: async (q) => (q.includes('COUNT') ? [{ n: 24 }] : [{ status: 'paid', n: 3 }, { status: 'shipped', n: 5 }]),
    call: async () => ({ status: 200, body: { keys: [{ name: 'dana-ro', scope: 'read' }] } }),
  };
  const t = {
    checks: [
      { name: 'loaded', kind: 'sql', sql: 'SELECT COUNT(*) AS n FROM t', expect: { n: 24 } },
      { name: 'counts', kind: 'answer_matches_sql', sql: 'SELECT status, n', columns: ['status', 'n'] },
      { name: 'key', kind: 'http', path: '/v1/admin/keys', test: "body.keys.some(k => /dana/.test(k.name) && k.scope === 'read')" },
      { name: 'ids', kind: 'answer_includes', any: ['t-1', 't-2', 't-3'], at_least: 2 },
    ],
  };
  const ok = await runChecks(t, { sandbox, outcome: 'done', answer: 'paid 3, shipped 5; tickets t-1 and t-3' });
  assert.equal(ok.success, true, JSON.stringify(ok.results));
  const gaveUp = await runChecks(t, { sandbox, outcome: 'gave_up', answer: 'paid 3, shipped 5; tickets t-1 and t-3' });
  assert.equal(gaveUp.success, false);
  const wrong = await runChecks(t, { sandbox, outcome: 'done', answer: 'paid 3, shipped 4; ticket t-1' });
  assert.equal(wrong.success, false);
  assert.deepEqual(wrong.results.map((r) => r.ok), [true, false, true, false]);
  const broken = await runChecks({ checks: [{ kind: 'sql', sql: 'x' }] }, { sandbox: { sql: async () => { throw new Error('no table'); } }, outcome: 'done', answer: '' });
  assert.equal(broken.success, false);
  assert.match(broken.results[0].detail, /no table/);
});

test('costs: priced per million tokens, cache reads cheap, the cap stops spending', () => {
  assert.equal(costOf('claude-opus-5-5', { input_tokens: 1e6, output_tokens: 1e6 }), 24);
  assert.equal(costOf('claude-haiku-5-5', { cache_read_input_tokens: 1e6 }), 0.01);
  assert.equal(costOf('unknown-model', { input_tokens: 1e6 }), 0);
  const s = new Spend(1);
  s.charge('claude-sonnet-5-5', { input_tokens: 400000 });
  assert.equal(s.over, false);
  s.charge('claude-sonnet-5-5', { input_tokens: 100000 });
  assert.equal(s.over, true);
  assert.equal(modelId('haiku'), 'claude-haiku-5-5');
  assert.equal(modelId('claude-opus-5-5'), 'claude-opus-5-5');
});

test('expand: personas × tasks × seeds, a model per seed, filters and limits', () => {
  const set = {
    seeds: 3, models: ['haiku', 'haiku', 'sonnet'],
    personas: [{ id: 'a' }, { id: 'b' }],
    tasks: [{ id: 'x' }, { id: 'y', personas: ['b'] }],
  };
  const all = expand(set);
  assert.equal(all.length, 2 * 3 + 1 * 3);
  assert.deepEqual(all.filter((j) => j.persona.id === 'a' && j.task.id === 'x').map((j) => j.model), ['claude-haiku-5-5', 'claude-haiku-5-5', 'claude-sonnet-5-5']);
  assert.equal(expand(set, { only: { persona: ['b'] } }).length, 6);
  assert.equal(expand(set, { only: { task: ['y'], seed: [2] } }).length, 1);
  assert.equal(expand(set, { limit: 4 }).length, 4);
  const r = rates([{ success: true, steps: 4, interview: { ease: 6 } }, { success: false, steps: 8, interview: { ease: 2 } }, { success: true, steps: 3 }]);
  assert.deepEqual(r, { n: 3, ok: 2, rate: 0.667, steps: 5, ease: 4 });
});

test('agent: acts through the surface, finishes, answers the interview', async () => {
  const model = scripted([
    { tool: 'look', input: { note: 'Let me see the page.' } },
    { tool: 'press', input: { note: 'Try Enter.', key: 'Enter' } },
    { tool: 'finish', input: { outcome: 'done', answer: 'It is t-11.', note: 'Found it.' } },
    { tool: 'interview', input: { ease: 5, hardest: 'none', expected: 'search', one_change: 'a search box', would_use: 'yes' } },
  ]);
  useClient(model);
  const surface = new FakeSurface({ press: { ok: false, error: 'covered' } });
  const steps = [];
  const spend = new Spend(5);
  const r = await runSession({ persona, task, surface, model: 'haiku', spend, onStep: (s) => steps.push(s) });
  assert.equal(r.outcome, 'done');
  assert.equal(r.answer, 'It is t-11.');
  assert.equal(r.interview.ease, 5);
  assert.equal(r.steps, 3);
  assert.deepEqual(steps.map((s) => [s.tool, s.ok]), [['look', true], ['press', false], ['finish', true]]);
  assert.equal(steps[0].thought, 'Let me see the page.');
  assert.deepEqual(steps[1].input, { key: 'Enter' });
  assert.ok(spend.calls === 4 && r.cost > 0);
  // Every request kept the persona in the system prompt and gave the tools.
  for (const p of model.requests) {
    assert.match(p.system, /Maya Test, a tester/);
    assert.ok(p.tools.some((t) => t.name === 'finish'));
  }
  // The failed step reached the model as an error result.
  const results = model.requests[2].messages.at(-1).content;
  assert.equal(results[0].is_error, true);
  // The interview was asked with its tool, after the finish's result.
  assert.ok(model.requests[3].tools.some((t) => t.name === 'interview'));
});

test('agent: stops at the step limit, and nudges a model that only talks', async () => {
  useClient(scripted(Array.from({ length: 10 }, () => ({ tool: 'look', input: { note: 'again' } })).concat([{ text: 'done?' }])));
  const r = await runSession({ persona, task, surface: new FakeSurface(), model: 'haiku', spend: new Spend(5), maxSteps: 3 });
  assert.equal(r.outcome, 'out_of_steps');
  assert.equal(r.steps, 3);

  useClient(scripted([{ text: 'I think it is t-11' }, { text: 'still talking' }, { text: 'and again' }, { text: 'interview?' }]));
  const t = await runSession({ persona, task, surface: new FakeSurface(), model: 'haiku', spend: new Spend(5) });
  assert.equal(t.outcome, 'no_action');
  assert.equal(t.steps, 0);
});

test('agent: a spent budget ends the session as "budget"', async () => {
  useClient(scripted([{ tool: 'look', input: { note: 'x' } }]));
  const spend = new Spend(0.000001);
  spend.usd = 1;
  const r = await runSession({ persona, task, surface: new FakeSurface(), model: 'haiku', spend });
  assert.equal(r.outcome, 'budget');
  assert.match(r.error, /budget/);
});

test('browser: Playwright\'s failures in a person\'s words', () => {
  assert.match(why('locator.click: Timeout 8000ms exceeded.\nCall log:\n  - <div class="modal-back"></div> intercepts pointer events\n'), /covered by another one.*<div class="modal-back">/);
  assert.match(why("locator.click: Timeout 8000ms exceeded.\nCall log:\n  - waiting for locator('aria-ref=e161')"), /no element has that reference/);
  assert.equal(why('page.goto: net::ERR_FAILED'), 'page.goto: net::ERR_FAILED');
});

test('browser: a part of the page whole, and an open dialog first when the page is cut', () => {
  const rows = Array.from({ length: 400 }, (_, i) => `      - row "t-${i}" [ref=e${100 + i}]`).join('\n');
  const tree = `- generic [ref=e1]:\n  - main [ref=e2]:\n    - table [ref=e3]:\n${rows}\n    - button "Create a key" [ref=e9]\n  - dialog [ref=e600]:\n    - textbox "Name" [ref=e601]\n    - button "Create" [ref=e602]`;
  assert.equal(subtree(tree, 'e600'), '  - dialog [ref=e600]:\n    - textbox "Name" [ref=e601]\n    - button "Create" [ref=e602]');
  assert.equal(subtree(tree, 'e3').split('\n').length, 401);
  assert.equal(subtree(tree, 'e999'), null);
  const page = fit(tree, 3000);
  assert.ok(page.startsWith('A dialog is open:\n  - dialog [ref=e600]'), page.slice(0, 80));
  assert.match(page, /textbox "Name" \[ref=e601\]/);
  assert.match(page, /the rest is cut: look with a part/);
  assert.ok(page.length < 3300);
  assert.equal(fit('- button "x" [ref=e1]', 3000), '- button "x" [ref=e1]');
});

test('report: a run as Markdown', () => {
  const md = markdown({
    run: { id: 'run-1', name: 'starter', status: 'done', started: '2026-11-12T00:00:00Z', set: { personas: ['a'], tasks: ['x'] }, cost: { usd: 1.5, users: { usd: 1 }, judge: { usd: 0.5 } }, totals: { sessions: 2, rate: 0.5, findings: 1, by_persona: { a: { rate: 0.5 } }, by_task: { x: { rate: 0.5 } }, by_model: { m: { n: 2, rate: 0.5, steps: 4, ease: 3 } } } },
    sessions: [{ id: 's1', persona: { id: 'a' }, task: { id: 'x' }, seed: 1, model: 'm', outcome: 'done', success: true, steps: 3, checks: [{ ok: true }], interview: { ease: 4 } }, { id: 's2', persona: { id: 'a' }, task: { id: 'x' }, seed: 2, model: 'm', outcome: 'gave_up', success: false, steps: 5, checks: [] }],
    findings: [{ rank: 1, title: 'Dates', category: 'missing_feature', severity: 2, sessions: ['s2'], share: 0.5, personas: ['a'], tasks: ['x'], summary: 'No intervals.', fix: 'Add them.', evidence: [{ session: 's2', step: 2, detail: 'refused' }] }],
  });
  assert.match(md, /\| a \| 1\/2 \| 50% \|/);
  assert.match(md, /### 1\. Dates/);
  assert.match(md, /s2 step 2: refused/);
  assert.match(md, /\$1\.50/);
});

test('agent: old pages are shortened in batches, so the prompt cache holds between them', async () => {
  const { trim } = await import('../src/agent.mjs');
  const page = (i) => ({ role: 'user', content: [{ type: 'tool_result', tool_use_id: `t${i}`, content: `page ${i} `.repeat(300) }] });
  const msgs = [];
  const changed = [];
  for (let i = 1; i <= 12; i++) { msgs.push(page(i)); changed.push(trim(msgs, 2)); }
  // Shortened at the 7th page and the 12th (2 kept + 4 slack), not at every step.
  assert.deepEqual(changed.map((c, i) => (c ? i + 1 : 0)).filter(Boolean), [7, 12]);
  const whole = msgs.filter((m) => m.content[0].content.startsWith('page'));
  assert.deepEqual(whole.map((m) => m.content[0].tool_use_id), ['t11', 't12']);
});
