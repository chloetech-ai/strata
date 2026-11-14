// A screen's PARAMS expressions, as the build check works them out before the
// screen runs (forge.mjs dryRunScreens): in QuickJS, never in the engine's
// own process (audit 4, X-G1). No model, no ChloeDB: a fake db.

import test from 'node:test';
import assert from 'node:assert/strict';
import { evalExpression } from '../engine/sandbox.mjs';
import { dryRunScreens } from '../engine/forge.mjs';
import { compileStatement } from '../engine/statement.mjs';

test('evalExpression: plain values from the screen\'s first state', async () => {
  const scope = { state: { q: 'hi', n: 2, tags: ['a'] }, today: '2026-11-14' };
  assert.equal(await evalExpression("state.q || ''", scope), 'hi');
  assert.equal(await evalExpression('state.n + 1', scope), 3);
  assert.deepEqual(await evalExpression('[state.tags, { x: 1 }]', scope), [['a'], { x: 1 }]);
  assert.equal(await evalExpression('today', scope), '2026-11-14');
  assert.equal(await evalExpression('app.today()', scope), '2026-11-14');
  assert.equal(await evalExpression('app.me()', scope), null);
  assert.equal(await evalExpression('lib.anything(1)', scope), null);
  assert.equal(await evalExpression('new Date(today).getUTCFullYear()', scope), 2026);
  assert.equal(await evalExpression('state.missing', scope), null);
});

test('evalExpression: nothing of the host is reachable', async () => {
  const scope = { state: {} };
  assert.equal(await evalExpression('typeof process', scope), 'undefined');
  assert.equal(await evalExpression('typeof require', scope), 'undefined');
  assert.equal(await evalExpression('typeof globalThis.process', scope), 'undefined');
  assert.equal(await evalExpression("import('node:child_process')", scope), undefined, 'a promise is no value');
  assert.equal(await evalExpression("require('node:fs').readFileSync('/etc/passwd', 'utf8')", scope), undefined);
  assert.equal(await evalExpression("process.env.STRATA_TOKEN", scope), undefined);
  // A deadline: an endless loop gives up, it does not hold the engine.
  const t0 = Date.now();
  assert.equal(await evalExpression('(() => { for (;;) {} })()', scope, { timeoutMs: 100 }), undefined);
  assert.ok(Date.now() - t0 < 2000);
  // Memory too.
  assert.equal(await evalExpression("'x'.repeat(1 << 30)", scope), undefined);
  // Not an expression at all.
  assert.equal(await evalExpression('1); (function () { return 2', scope), undefined);
});

test('dryRunScreens: a PARAMS expression that reaches for the host runs nothing on it', async () => {
  const marker = `sa-xg1-${process.pid}-${Date.now()}`;
  delete globalThis[marker];
  const screen = compileStatement(`CREATE SCREEN items TITLE 'Items'
DATA rows AS (SELECT id, name FROM items WHERE name = :q) PARAMS (q = (globalThis['${marker}'] = 1, typeof process !== 'undefined' ? process.env.HOME : 'safe'))
DATA two AS (SELECT id FROM items WHERE name = :q) PARAMS (q = state.word)
STATE word = "pear"`);
  const seen = [];
  const db = {
    explain: async () => ({ ir: { kind: 'scan', table: 'app_x1_items' } }),
    query: async (sql, params) => { seen.push({ sql, params }); return { rows: [] }; },
  };
  const plan = { views: [{ slug: 'items', screen: screen.screen || screen }] };
  const problems = await dryRunScreens(db, 'x1', ['items'], plan);
  assert.deepEqual(problems, []);
  assert.equal(globalThis[marker], undefined, 'the expression did not run in this process');
  const params = seen.map((s) => s.params.q);
  assert.ok(!params.includes(process.env.HOME), 'the host\'s environment was not read');
  assert.ok(params.includes('pear'), 'a plain expression is still worked out');
});
