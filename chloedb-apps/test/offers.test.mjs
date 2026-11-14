// Library pieces a mature builder's apps have: things to order or book while
// some are left (stock, seats, places), and a course's lessons with progress.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import { compileStatement } from '../engine/statement.mjs';
import { expandScreen } from '../engine/patterns.mjs';
import { checkScreen } from '../engine/forge.mjs';

// Evaluates a screen's "=..." expression with the names given.
const run = (expr, scope) => vm.runInNewContext(`(${expr.slice(1)})`, scope);

test('OFFERS: a card per row, a few left, sold out with no action, the action opens the order', () => {
  const { screen } = compileStatement(`CREATE SCREEN shop TITLE 'Shop'
PAGE front
OFFERS candles
  LEFT stock  PRICE price  IMAGE image  TEXT notes
  SOON 2  OUT 'Sold out'  ACTION 'Order'  OPEN order
  TITLE 'The shelf'`);
  const ex = expandScreen(screen);
  assert.deepEqual(ex.problems, []);
  assert.deepEqual(ex.used, ['offers']);
  // A pattern on a front page takes the id a nav link goes to.
  assert.equal(expandScreen({ body: [{ use: 'offers', table: 't', left: 'n', id: 'shop' }] }).screen.body[0].id, 'shop');
  assert.deepEqual(checkScreen(ex.screen), []);
  const s = ex.screen;
  assert.equal(s.data.candles.sql, 'SELECT * FROM candles ORDER BY name LIMIT 60');
  const car = s.body[0];
  assert.equal(car.title, 'The shelf');
  // The lets, as the runtime computes them.
  const scope = { app: { fmt: { money: (n) => `$${n}` }, toast() {}, call: async () => {} } };
  for (const [k, v] of Object.entries(s.let)) scope[k] = run(v, scope);
  const item = (stock) => ({ id: 'c1', name: 'Fig', stock, price: 18 });
  assert.equal(run(car.item.badge, { ...scope, item: item(0) }), 'Sold out');
  assert.equal(run(car.item.badge, { ...scope, item: item(2) }), 'Only 2 left');
  assert.equal(run(car.item.badge, { ...scope, item: item(9) }), null);
  assert.equal(run(car.item.action, { ...scope, item: item(0) }), null, 'none left: no action');
  assert.deepEqual(JSON.parse(JSON.stringify(run(car.item.action, { ...scope, item: item(5) }))), { label: 'Order', open: 'order', params: { id: 'c1' } });
  assert.equal(run(car.item.price, { ...scope, item: item(5) }), '$18');
});

test('OFFERS with a date and CALL: coming ones first, one tap books through the rule', async () => {
  const { screen } = compileStatement(`CREATE SCREEN races TITLE 'Races'
OFFERS races
  LEFT places_left  DATE day  WHERE day >= :today  ORDER BY day
  OUT 'Full'  ACTION 'Sign up'  CALL sign-up  DONE 'You are in'`);
  const ex = expandScreen(screen);
  assert.deepEqual(ex.problems, []);
  assert.deepEqual(checkScreen(ex.screen), []);
  assert.equal(ex.screen.data.races.sql, 'SELECT * FROM races WHERE day >= :today ORDER BY day LIMIT 60');
  const calls = [];
  const scope = { app: { fmt: { date: (d) => `on ${d}` }, toast: (t) => calls.push(['toast', t]), call: async (slug, input) => { calls.push([slug, input]); } } };
  for (const [k, v] of Object.entries(ex.screen.let)) scope[k] = run(v, scope);
  const car = ex.screen.body[0];
  const race = { id: 'r1', name: 'Harbour 10K', day: '2026-11-27', places_left: 4 };
  assert.equal(run(car.item.eyebrow, { ...scope, item: race }), 'on 2026-11-27');
  const act = run(car.item.action, { ...scope, item: race });
  assert.equal(act.label, 'Sign up');
  await act.do();
  assert.deepEqual(JSON.parse(JSON.stringify(calls)), [['sign-up', { id: 'r1' }], ['toast', 'You are in']]);
  assert.equal(run(car.item.badge, { ...scope, item: { ...race, places_left: 0 } }), 'Full');
  assert.throws(() => compileStatement('CREATE SCREEN x\nOFFERS races\n  OPEN book'), /needs LEFT/);
});

test('LESSONS: progress, the next lesson as the main action, the certificate at the end', () => {
  const { screen } = compileStatement(`CREATE SCREEN course TITLE 'Course'
LAYOUT editorial
LESSONS lessons
  PROGRESS completions  LESSON lesson_id  MINUTES minutes  ORDER BY position
  OPEN lesson  CERTIFICATE certificate`);
  const ex = expandScreen(screen);
  assert.deepEqual(ex.problems, []);
  assert.deepEqual(checkScreen(ex.screen), []);
  const s = ex.screen;
  assert.match(s.data.lessons_done, /SELECT lesson_id AS lesson FROM completions/);
  const lessons = [{ id: 'a', title: 'Starter', position: 1 }, { id: 'b', title: 'Dough', position: 2 }, { id: 'c', title: 'Bake', position: 3 }];
  const opened = [];
  const at = (done) => {
    const scope = { lessons, lessons_done: done.map((lesson) => ({ lesson })), app: { open: (v, p) => opened.push([v, p]), me: () => ({ id: 'u' }) } };
    for (const [k, v] of Object.entries(s.let)) scope[k] = run(v, scope);
    return scope;
  };
  const one = at(['a']);
  assert.equal(one.lessons_n, 1);
  assert.equal(one.lessons_next.id, 'b');
  const next = s.body[1].then[1];
  const card = next.then[0].card;
  assert.equal(run(card.at(-1).button, one), 'Start lesson 2');
  run(card.at(-1).do, one)();
  assert.deepEqual(JSON.parse(JSON.stringify(opened)), [['lesson', { id: 'b' }]]);
  const all = at(['a', 'b', 'c']);
  assert.equal(all.lessons_next, null);
  assert.equal(next.else[0].card[1].button, 'See your certificate');
});
