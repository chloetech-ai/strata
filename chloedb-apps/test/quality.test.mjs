// The checks that are numbers (engine/quality.mjs), the drawn-page rules
// that use them (engine/look.mjs), and the LAYOUT clause of CREATE SCREEN.
import test from 'node:test';
import assert from 'node:assert/strict';
import { Q, QUALITY_SRC, parseLayout, LAYOUT_NAMES, labelProblems, designProblems, mainAction, staticChecks, screenLabels } from '../engine/quality.mjs';
import { rules, repairRequest, checkStatic, namesOf } from '../engine/look.mjs';
import { compileStatement, screenStatement, STATEMENT_DOC } from '../engine/statement.mjs';
import { cleanArt, SYSTEM } from '../engine/forge.mjs';
import { shapeDesigns, designPrompt } from '../engine/designs.mjs';
import { checkDesign } from '../web/design.js';

const near = (a, b, d = 0.01) => assert.ok(Math.abs(a - b) < d, `${a} is not near ${b}`);

test('contrast: the WCAG ratio, with translucent text over its background', () => {
  near(Q.contrastRatio('#000000', '#ffffff'), 21);
  near(Q.contrastRatio('#ffffff', '#ffffff'), 1);
  near(Q.contrastRatio('rgb(118, 118, 118)', '#fff'), 4.54);
  // White on a light blue: the "Start learning" button that did not read.
  assert.ok(Q.contrastRatio('#ffffff', '#7aa2ff') < 3);
  // Half-transparent black on white is a mid grey.
  near(Q.contrastRatio('rgba(0, 0, 0, 0.5)', '#ffffff'), 3.98);
  near(Q.contrastRatio('#0008', '#fff'), Q.contrastRatio('rgba(0,0,0,0.533)', '#fff'), 0.02);
  assert.equal(Q.contrastRatio('nonsense', '#fff'), null);
  // Large text needs 3:1, other text 4.5:1.
  assert.equal(Q.neededContrast(16, 400), 4.5);
  assert.equal(Q.neededContrast(24, 400), 3);
  assert.equal(Q.neededContrast(19, 700), 3);
  assert.equal(Q.neededContrast(19, 600), 4.5);
});

test('contrast: the colors of a gradient, never of a picture', () => {
  assert.deepEqual(Q.gradientColors('linear-gradient(125deg, rgb(47, 91, 211), rgba(0, 0, 0, 0.5) 55%, #ffffff)'), ['rgb(47, 91, 211)', 'rgba(0, 0, 0, 0.5)', '#ffffff']);
  assert.deepEqual(Q.gradientColors('url("a.jpg")'), []);
  assert.deepEqual(Q.gradientColors('linear-gradient(red, blue), url(a.jpg)'), []);
  assert.deepEqual(Q.gradientColors('none'), []);
});

test('target size: 44 by 44 px, half a pixel of rounding allowed', () => {
  assert.equal(Q.smallTarget(44, 44), false);
  assert.equal(Q.smallTarget(43.6, 120), false);
  assert.equal(Q.smallTarget(120, 32), true);
  assert.equal(Q.smallTarget(40, 44), true);
  assert.equal(Q.smallTarget(30, 30, 24), false);
});

test('labels: raw column names, placeholders and a generic word beside a number', () => {
  assert.match(Q.rawLabel('Groups 4'), /generic word/);
  assert.match(Q.rawLabel('Items: 12'), /generic word/);
  assert.match(Q.rawLabel('Groups', { value: '4' }), /generic word/);
  assert.match(Q.rawLabel('lesson_count'), /snake_case/);
  assert.match(Q.rawLabel('quizScore'), /camelCase/);
  assert.match(Q.rawLabel('Item 1'), /placeholder/);
  assert.match(Q.rawLabel('Lorem'), /placeholder/);
  assert.match(Q.rawLabel('enrollments', { names: ['enrollments'] }), /table's name/);
  assert.match(Q.rawLabel('Enrollments 3', { names: ['enrollment'] }), /generic word/);
  // Words for people pass.
  for (const ok of ['Lessons done', 'Groups meeting this week', 'Coming', 'Owed', 'Start lesson 1', 'Carnitas', 'RSVP', '']) assert.equal(Q.rawLabel(ok, { value: 3 }), null, ok);
  assert.equal(Q.rawLabel('Groups'), null); // a heading, no number beside it
});

test('the kit runs in a page as it does here', () => {
  const g = {};
  new Function('window', QUALITY_SRC)(g);
  near(g.__q.contrastRatio('#000', '#fff'), 21);
  assert.equal(g.__q.smallTarget(10, 10), true);
  assert.match(g.__q.rawLabel('Groups 4'), /generic/);
});

test('a screen\'s labels and its main action, read without drawing it', () => {
  const screen = { body: [
    { h: 'Your groups' },
    { kpis: [{ label: 'Groups', of: 'n' }, { label: 'Meeting this week', value: '=3' }] },
    { table: '=rows', columns: [{ key: 'member_count', label: 'member_count' }, { key: 'name', label: 'Group' }] },
    { form: 'groups', fields: [{ name: 'name' }], submit: 'Add group' },
  ] };
  const labels = screenLabels(screen).map((l) => l.text);
  assert.deepEqual(labels, ['Your groups', 'Groups', 'Meeting this week', 'member_count', 'Group']);
  const p = labelProblems(screen, ['groups', 'member_count']);
  assert.equal(p.length, 2);
  assert.match(p[0], /big number label "Groups"/);
  assert.match(p[1], /column label "member_count"/);
  assert.deepEqual(mainAction(screen), { kind: 'form', label: 'Add group' });
  assert.deepEqual(mainAction({ body: [{ nav: 'X' }, { hero: 'Tacos', actions: [{ label: 'See the menu', open: 'menu' }] }] }), { kind: 'hero', label: 'See the menu' });
  assert.equal(mainAction({ body: [{ h: 'Hi' }, 'Words'] }), null);
});

test('a design\'s own colors are measured: the words on its buttons read', () => {
  assert.deepEqual(designProblems(checkDesign({ light: { bg: '#ffffff', card: '#ffffff', fg: '#111111', accent: '#2f5bd3' } })), []);
  // checkDesign makes every pair readable, so a design as stored passes; a
  // raw design that skipped it (a hand-made one) is caught.
  assert.ok(designProblems(checkDesign({ light: { bg: '#ffffff', card: '#ffffff', fg: '#bbbbbb', accent: '#9ec5ff' } })).length === 0);
  const found = staticChecks({ screens: [{ slug: 'home', title: 'Home', screen: { body: [{ kpis: [{ label: 'lesson_count', value: 3 }] }] } }], names: [] });
  assert.ok(found.some((f) => f.by === 'static' && f.severity === 'high' && /lesson_count/.test(f.problem)));
  assert.ok(found.some((f) => /no screen has a main action/.test(f.problem) && f.severity === 'low'));
  assert.match(repairRequest(found), /checked without drawing them/);
});

test('the drawn-page rules: button words, the main action below the fold, raw labels', () => {
  const m = { text: 100, controls: 4, width: 390, scroll: 390, screen: 844, front: true, firstTop: null, small: [], smallCount: 0, repeats: [], faint: [{ text: 'Start learning', ratio: 2.1, need: 4.5 }, { text: 'Small print', ratio: 3.9, need: 4.5 }], faintCount: 2, faintButtons: [{ text: 'Start learning', ratio: 2.1, need: 4.5 }], ids: [], tinyFields: 0, numbers: 0, zeros: 0, rowsShown: 0, mainTop: 1200, mainName: 'Order tacos', raw: ['"Groups 4" (one generic word beside a number)'] };
  const out = rules(m, 'phone');
  const said = out.map((r) => r.problem).join('\n');
  assert.match(said, /button words that are hard to read: "Start learning" at 2.1:1/);
  assert.match(said, /text that is hard to read: "Small print"/);
  assert.doesNotMatch(said, /text that is hard to read: "Start learning"/);
  assert.match(said, /main action \("Order tacos"\) starts 1200 px down/);
  assert.match(said, /labels that are not words for people: "Groups 4"/);
  assert.ok(out.every((r) => r.severity === 'high'));
  // On a computer the fold is not a phone's: no main-action rule.
  assert.doesNotMatch(rules(m, 'computer').map((r) => r.problem).join('\n'), /main action/);
  // A small target on a phone.
  assert.match(rules({ ...m, faint: [], faintCount: 0, faintButtons: [], raw: [], mainTop: 300, small: ['Menu (40×40 px)'], smallCount: 1 }, 'phone')[0].problem, /smaller than a finger \(44 px\): Menu/);
});

test('the static fallback reads screens stored as JSON in a view\'s code', () => {
  const view = { tables: [{ spec: { name: 'lessons', columns: [{ name: 'title' }, { name: 'done_count' }] } }],
    views: [{ slug: 'home', title: 'Home', spec: { screen: true }, code: JSON.stringify({ body: [{ kpis: [{ label: 'Lessons', of: 'k' }] }, { form: 'lessons', fields: ['title'], submit: 'Add lesson' }] }) }] };
  assert.deepEqual(namesOf(view), ['lessons', 'title', 'done_count']);
  const found = checkStatic(view);
  assert.equal(found.length, 1);
  assert.match(found[0].problem, /"Lessons"/);
});

test('layouts: parsed from what a statement or a plan says', () => {
  assert.deepEqual(LAYOUT_NAMES, ['hero', 'split', 'editorial', 'dashboard', 'gallery', 'tool']);
  assert.equal(parseLayout('split'), 'split');
  assert.equal(parseLayout('Editorial, the menu as a chalkboard list'), 'editorial');
  assert.equal(parseLayout('single-tool'), 'tool');
  assert.equal(parseLayout('landing'), 'hero');
  assert.equal(parseLayout('a gallery of the dishes'), 'gallery');
  assert.equal(parseLayout('cards'), 'gallery');
  assert.equal(parseLayout('whatever'), null);
  assert.equal(parseLayout(3), null);
});

test('CREATE SCREEN: LAYOUT and PAGE front LAYOUT, and back to a statement', () => {
  const a = compileStatement("CREATE SCREEN lessons TITLE 'Lessons'\nLAYOUT split\nHEADING 'Lessons'");
  assert.equal(a.screen.layout, 'split');
  const b = compileStatement("CREATE SCREEN home TITLE 'Home'\nPAGE front LAYOUT editorial\nHEADING 'Tacos'");
  assert.equal(b.screen.page, 'front');
  assert.equal(b.screen.layout, 'editorial');
  // An unknown layout is left out, never a failed build.
  assert.equal(compileStatement("CREATE SCREEN x TITLE 'X'\nLAYOUT bento\nHEADING 'X'").screen.layout, undefined);
  assert.equal(compileStatement("CREATE SCREEN x TITLE 'X'\nLAYOUT 'Single-tool'\nHEADING 'X'").screen.layout, 'tool');
  const back = screenStatement('lessons', 'Lessons', a.screen);
  assert.match(back, /^LAYOUT split$/m);
  assert.deepEqual(compileStatement(back).screen, a.screen);
  assert.equal(screenStatement('x', 'X', { body: [], layout: 'bento' }), null);
  assert.match(STATEMENT_DOC, /LAYOUT <hero \| split \| editorial \| dashboard \| gallery \| tool>/);
});

test('the art direction: layout, type, accent, imagery and the one main action', () => {
  const art = cleanArt({ concept: 'A chalkboard menu by the window', mood: 'warm, quick, local', layout: 'Editorial: the menu as a chalkboard list', type: 'serif titles over plain text', accent: 'salsa red', imagery: 'the tacos on the plate', action: 'See the menu' });
  assert.equal(art.structure, 'editorial');
  assert.equal(art.type, 'serif');
  assert.equal(art.action, 'See the menu');
  assert.equal(cleanArt({ concept: 'x' }).structure, '');
  // The forge sees the rules, compactly.
  for (const rule of [/44 by 44 px/, /4\.5:1/, /first phone screen/, /"Groups 4"/, /never a generic one/, /real product itself/, /"layout": the first screen's structure/]) assert.match(SYSTEM, rule);
  // The designs follow it: an editorial page is a reading column.
  const designs = [checkDesign({ look: 'native', width: 'wide' }), checkDesign({ look: 'bold' }), checkDesign({ look: 'plain', nav: 'sidebar' })];
  shapeDesigns(designs, { art });
  assert.equal(designs[0].width, 'narrow');
  assert.equal(designs[0].titles, 'serif');
  assert.match(designPrompt({ name: 'Tacos', description: 'A taco truck' }, null, { art }), /main action: See the menu/);
});
