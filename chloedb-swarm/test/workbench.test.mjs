// The Workbench runner (src/workbench.mjs) on a fake surface and a scripted
// model: no browser, no API, no `claude`. It does not import model.mjs, so
// it runs without `npm install` too.

import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { play, parseJson, browserFindings, stepPrompt, personaText } from '../src/workbench.mjs';
import { FakeSurface } from './helpers.mjs';

const job = { url: 'http://127.0.0.1:1/preview/a1', token: 't', persona: { name: 'Maya Chen', role: 'a first-time visitor', traits: ['Skims.'] }, task: 'Buy a mug', model: 'haiku', max_steps: 5 };

function model(answers) {
  const asked = [];
  return { name: 'fake', cost: 0.02, asked, async ask(q) { asked.push(q); const a = answers.shift(); if (a instanceof Error) throw a; return a; } };
}

test('workbench: plays a job, reports steps, browser findings, the review and the end', async () => {
  const surface = new FakeSurface({ click: { text: 'the cart', ok: true, error: null, url: 'http://x/cart', events: [{ kind: 'exception', text: 'TypeError: total is undefined' }, { kind: 'http', text: '500 POST /api/orders' }] } });
  const m = model([
    { note: 'Tapping the mug', action: 'click', ref: 'e3' },
    { note: 'Done, it is in my cart', action: 'finish', outcome: 'done', answer: 'One mug in the cart' },
    { findings: [{ kind: 'confusing', severity: 2, title: 'The total shows a dash', detail: 'No price at checkout', step: 1 }] },
  ]);
  const lines = [];
  const done = await play(job, surface, m, (o) => lines.push(o));
  assert.deepEqual(lines.map((l) => l.type), ['start', 'step', 'step', 'finding', 'finding', 'finding', 'done']);
  assert.equal(lines[1].action, 'click');
  assert.equal(lines[1].input.ref, 'e3');
  assert.equal(lines[1].thought, 'Tapping the mug');
  assert.deepEqual(surface.calls, [['click', { ref: 'e3' }]]);
  const titles = lines.filter((l) => l.type === 'finding').map((l) => l.title);
  assert.ok(titles[0].startsWith('The page threw: TypeError'), titles[0]);
  assert.equal(titles[1], 'The server answered 500 to POST /api/orders');
  assert.equal(titles[2], 'The total shows a dash');
  assert.equal(lines[4].severity, 3);
  assert.deepEqual({ outcome: done.outcome, answer: done.answer, cost_usd: done.cost_usd }, { outcome: 'done', answer: 'One mug in the cart', cost_usd: 0.02 });
  // The second step's prompt recalls the first, and the persona.
  assert.match(m.asked[1].prompt, /1\. clicked e3 \(“Tapping the mug”\)/);
  assert.match(m.asked[1].prompt, /You are Maya Chen, a first-time visitor\./);
  // The persona checks its arithmetic before it calls a total wrong, and
  // does the task's main action (Sixth build); the review holds it to that.
  assert.match(m.asked[0].system, /check your arithmetic against everything the page shows/);
  assert.match(m.asked[0].system, /Do the task's main action \(Pay/);
  assert.match(m.asked[2].system, /A wrong number is a bug only when you checked it/);
  assert.match(m.asked[2].system, /never did the task's main action/);
});

test('workbench: the step limit ends a run, and giving up is a finding', async () => {
  const lines = [];
  const d = await play({ ...job, max_steps: 2 }, new FakeSurface(), model([{ note: 'a', action: 'look' }, { note: 'b', action: 'scroll', direction: 'down' }, { findings: [] }]), (o) => lines.push(o));
  assert.equal(d.outcome, 'steps');
  const g = [];
  await play(job, new FakeSurface(), model([{ note: 'no idea', action: 'finish', outcome: 'gave_up', answer: 'I could not find the cart' }, { findings: [] }]), (o) => g.push(o));
  const f = g.find((l) => l.type === 'finding');
  assert.equal(f.severity, 3);
  assert.match(f.title, /gave up/);
});

test('workbench: a model that fails ends the run as an error', async () => {
  const lines = [];
  const d = await play(job, new FakeSurface(), model([new Error('claude -p: not logged in')]), (o) => lines.push(o));
  assert.equal(d.outcome, 'error');
  assert.ok(lines.some((l) => l.type === 'error' && /not logged in/.test(l.message)));
});

test('workbench: JSON out of words, repeated errors merged, persona text', () => {
  assert.deepEqual(parseJson('Sure:\n```json\n{"action":"look"}\n```'), { action: 'look' });
  assert.deepEqual(parseJson('{"a":1} trailing'), { a: 1 });
  assert.equal(parseJson('nothing'), null);
  const f = browserFindings([{ step: 1, action: 'look', events: [{ kind: 'exception', text: 'x at 12' }] }, { step: 2, action: 'look', events: [{ kind: 'exception', text: 'x at 13' }, { kind: 'http', text: '404 GET /a' }] }]);
  assert.equal(f.length, 1);
  assert.match(f[0].detail, /2 times/);
  assert.equal(personaText({ text: 'Someone in a rush' }), 'Someone in a rush');
  assert.match(stepPrompt(job, [], 'PAGE'), /Your task: Buy a mug[\s\S]*PAGE/);
});

test('workbench: --check answers JSON with ok and why', () => {
  const script = fileURLToPath(new URL('../src/workbench.mjs', import.meta.url));
  const r = spawnSync(process.execPath, [script, '--check', '--claude', '/nonexistent/claude'], { encoding: 'utf8', env: { ...process.env, CHLOEDB_SWARM_ANTHROPIC_KEY: '', ANTHROPIC_API_KEY: '' } });
  const v = JSON.parse(r.stdout.trim());
  assert.equal(typeof v.ok, 'boolean');
  assert.equal(v.ok, false);
  assert.equal(typeof v.why, 'string');
});
