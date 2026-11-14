// A page structure for every screen: the one the model chose, else the one
// that fits what the screen is for.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { layoutFor, assignLayouts } from '../engine/layouts.mjs';
import { compileStatement } from '../engine/statement.mjs';
import { prepareScreens } from '../engine/forge.mjs';

test('a screen that names no layout gets the one that fits it', () => {
  const art = { structure: 'editorial' };
  assert.equal(layoutFor({ page: 'front', body: [{ hero: 'Hi', photo: 'x' }] }, { art }), 'editorial', "a front page takes the art direction's");
  assert.equal(layoutFor({ page: 'front', body: [{ hero: 'Hi', look: 'split' }] }), 'split');
  assert.equal(layoutFor({ page: 'front', body: [{ hero: 'Hi' }] }, { art: { structure: 'dashboard' } }), 'hero', 'a front page is not a dashboard');
  assert.equal(layoutFor({ body: [] }, { patterns: ['trend'] }), 'dashboard');
  assert.equal(layoutFor({ body: [] }, { patterns: ['checks'] }), 'tool');
  assert.equal(layoutFor({ body: [] }, { patterns: ['detail'] }), 'editorial');
  assert.equal(layoutFor({ body: [] }, { patterns: ['board'] }), null, 'a board needs the width');
  assert.equal(layoutFor({ body: [{ gallery: '=rows', item: {} }] }), 'gallery');
  assert.equal(layoutFor({ body: [{ kpis: [{ label: 'a' }, { label: 'b' }] }, { ui: 'barChart', props: {} }] }), 'dashboard');
  assert.equal(layoutFor({ body: [{ h: 'Quick add' }, { form: 'notes', fields: [{ name: 'text' }] }] }), 'tool');
  assert.equal(layoutFor({ body: [{ h: 'Lessons' }, { each: '=lessons', as: 'l', body: [{ h: '=l.title' }, { text: '=l.summary' }] }] }), 'editorial');
  assert.equal(layoutFor({ body: [{ table: '=rows' }, { form: 'x', fields: [] }] }, { patterns: ['crud'] }), null);
});

test('assignLayouts keeps a chosen layout and writes a given one into the statement', () => {
  const plan = { views: [
    { slug: 'home', screen: { page: 'front', layout: 'gallery', body: [] } },
    { slug: 'stats', screen: { body: [] }, patterns: ['dashboard'], statement: "CREATE SCREEN stats TITLE 'Stats'\nDASHBOARD sales" },
    { slug: 'site', screen: { page: 'front', body: [{ hero: 'Hi', look: 'split' }] }, statement: "CREATE SCREEN site\nPAGE front\nBLOCKS []" },
    { slug: 'odd', screen: { layout: 'nonsense', body: [] }, patterns: ['quiz'] },
  ] };
  assert.deepEqual(assignLayouts(plan), { stats: 'dashboard', site: 'split', odd: 'tool' });
  assert.equal(plan.views[0].screen.layout, 'gallery');
  assert.equal(compileStatement(plan.views[1].statement).screen.layout, 'dashboard');
  assert.equal(compileStatement(plan.views[2].statement).screen.layout, 'split');
});

test('a plan of statements comes out with a layout on every screen that fits one', () => {
  const plan = { art: { concept: 'a chalkboard', layout: 'editorial, a menu' }, views: [
    { slug: 'home', title: 'Home', order: 0 },
    { slug: 'trend', title: 'Trend', order: 1 },
  ] };
  const code = {
    home: "CREATE SCREEN home TITLE 'Home'\nPAGE front\nBLOCKS [{\"hero\": \"Tacos\"}]",
    trend: "CREATE SCREEN trend TITLE 'Sales'\nTREND orders\n  DATE created  VALUE SUM(total)  PER week",
  };
  const problems = prepareScreens(plan, code, { views: [], tables: [], app: null });
  assert.deepEqual(problems, []);
  assert.equal(plan.views[0].screen.layout, 'editorial');
  assert.equal(plan.views[1].screen.layout, 'dashboard');
  assert.match(code.trend, /"layout": "dashboard"/);
});

test('public pages and owner pages: a view says who it is for, and visitors never get the owner\'s', async () => {
  const { audienceOf, forVisitors } = await import('../engine/audience.mjs');
  const app = { views: [{ slug: 'manage', spec: { for: 'owner' } }] };
  assert.deepEqual(audienceOf({ slug: 'manage', for: 'admins' }, app), { for: 'owner' });
  assert.deepEqual(audienceOf({ slug: 'roster', for: 'Members' }, app), { for: 'members' });
  assert.deepEqual(audienceOf({ slug: 'home', for: 'everyone' }, app), {});
  assert.deepEqual(audienceOf({ slug: 'manage' }, app), { for: 'owner' }, 'a rewrite keeps who it was for');
  const views = [{ slug: 'home', spec: {} }, { slug: 'roster', spec: { for: 'members' } }, { slug: 'manage', spec: { for: 'owner' } }];
  assert.deepEqual(forVisitors(views).map((v) => v.slug), ['home', 'roster']);
});

test('FOR in a statement says who a screen is for', async () => {
  const { compileStatements } = await import('../engine/statement.mjs');
  const plan = { views: [{ slug: 'manage' }, { slug: 'home' }] };
  const code = { manage: "CREATE SCREEN manage TITLE 'Manage'\nFOR owner\nHEADING 'Races'", home: "CREATE SCREEN home\nHEADING 'Hi'" };
  assert.deepEqual(compileStatements(plan, code), []);
  assert.equal(plan.views[0].for, 'owner');
  assert.equal(plan.views[1].for, undefined);
  assert.throws(() => compileStatement("CREATE SCREEN x\nFOR strangers"), /FOR owner \| members \| everyone/);
});
