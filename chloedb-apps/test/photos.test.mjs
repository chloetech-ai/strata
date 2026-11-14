// Photos true to the product: the subject and its kind, Wikimedia Commons
// with credits, files kept with the app, and a drawn picture last. No network:
// fetch is replaced by a fake Commons.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { queryOf, commonsQueries, commonsCandidate, findPhoto, placePhotos, matchBuiltin, drawnPicture, accentHue, photoCache } from '../engine/photos.mjs';

const page = (pageid, title, { w = 3000, h = 2000, license = 'CC BY-SA 4.0', artist = '<a href="x">Ana Ruiz</a>', desc = '', mime = 'image/jpeg', index = 1 } = {}) => ({
  pageid, index, title: `File:${title}.jpg`,
  imageinfo: [{ mime, width: w, height: h, thumburl: `https://upload.example/${pageid}.jpg`, descriptionurl: `https://commons.wikimedia.org/wiki/File:${pageid}`,
    extmetadata: { LicenseShortName: { value: license }, Artist: { value: artist }, ImageDescription: { value: desc } } }],
});

// A fake Commons: each search's words -> its pages; photos are 3 bytes.
function fakeWeb(bySearch) {
  const asked = [];
  const real = globalThis.fetch;
  globalThis.fetch = async (url) => {
    const u = String(url);
    asked.push(u);
    if (u.startsWith('https://upload.example/')) return new Response(new Uint8Array([1, 2, 3]), { headers: { 'content-type': 'image/jpeg' } });
    const q = decodeURIComponent(new URL(u).searchParams.get('gsrsearch') || '').replace(/ filemime:image\/jpeg$/, '');
    const pages = bySearch[q] || [];
    return new Response(JSON.stringify({ query: { pages: Object.fromEntries(pages.map((p) => [p.pageid, p])) } }), { headers: { 'content-type': 'application/json' } });
  };
  return { asked, restore: () => { globalThis.fetch = real; } };
}

test('a query names its subject and the words that say which kind of it', () => {
  assert.deepEqual(queryOf('taco veggie').kind, ['taco', 'veggi']);
  assert.equal(queryOf('taco veggie').subject, 'taco');
  // Where a thing is does not say which thing it is.
  assert.deepEqual(queryOf('tacos on a plate').kind, ['taco']);
  // A loose word is never the subject.
  assert.equal(queryOf('group of runners on a river path').subject, 'runner');
  assert.equal(queryOf('dark roast coffee').subject, 'roast');
  assert.deepEqual(commonsQueries('handmade ceramic bowl on a shelf'), ['ceramic bowl shelf', 'ceramic bowl', 'ceramic']);
  // The built-in set's one taco photo is a taco, but not a veggie taco.
  assert.ok(matchBuiltin('tacos on a plate', { strict: true }).length);
  assert.equal(matchBuiltin('taco veggie', { strict: true }).length, 0);
  // Half of the kind at least: a salad bowl is no ceramic speckled bowl.
  assert.equal(matchBuiltin('bowl ceramic speckled', { product: true }).length, 0);
  assert.ok(matchBuiltin("hairdresser cutting woman's hair in bright salon").length, 'a scene fits more loosely');
});

test('a Commons file fits when it is a large enough photo whose title has the subject', () => {
  const ok = commonsCandidate(page(1, 'Veggie tacos in Austin'), 'taco veggie', 'card');
  assert.ok(ok && ok.whole);
  assert.equal(ok.credit, 'Veggie tacos in Austin by Ana Ruiz, CC BY-SA 4.0, Wikimedia Commons');
  assert.equal(ok.free, false);
  assert.equal(commonsCandidate(page(2, 'Garden in spring', { desc: 'a taco stand' }), 'taco', 'card'), null, 'the subject only in the description');
  assert.equal(commonsCandidate(page(3, 'Iron Age vessel (FindID 1234) bowl'), 'bowl', 'card'), null, 'a finds catalogue');
  assert.equal(commonsCandidate(page(4, 'Taco', { w: 500, h: 400 }), 'taco', 'card'), null, 'too small');
  assert.equal(commonsCandidate(page(5, 'Taco', { w: 2000, h: 3000 }), 'taco', 'hero'), null, 'a hero is wide');
  assert.equal(commonsCandidate(page(6, 'Taco', { mime: 'image/png' }), 'taco', 'card'), null, 'not a photo');
  assert.equal(commonsCandidate(page(7, 'Taco', { license: 'No restrictions' }), 'taco', 'card'), null, 'a scan of an archive');
  const plant = { ...page(10, 'Golden candle in bloom'), categories: [{ title: 'Category:Flowers of Brazil' }] };
  assert.equal(commonsCandidate(plant, 'candle', 'card'), null, 'a plant named like the thing');
  assert.ok(commonsCandidate(plant, 'candle flower', 'card'), 'unless a plant is asked for');
  const cc0 = commonsCandidate(page(8, 'Taco al pastor', { license: 'CC0' }), 'taco', 'card');
  assert.ok(cc0.free, 'public domain needs no credit');
  assert.ok(cc0.score > commonsCandidate(page(9, 'Taco al pastor'), 'taco', 'card').score);
});

test('findPhoto: the built-in set when it has the kind, Commons when it does not, nothing of another thing', async () => {
  const web = fakeWeb({
    'taco veggie': [page(10, 'Taco salad bowl', { index: 1 }), page(11, 'Veggie tacos', { index: 2 })],
    'candle lavender': [page(20, 'Candle in a church')],
    candle: [page(21, 'Candle in a church')],
  });
  try {
    // The built-in tacos photo, for tacos.
    assert.equal((await findPhoto('tacos on a plate', { use: 'hero' })).source, 'builtin');
    // A veggie taco is a photo of veggie tacos, from Commons, credited.
    const v = await findPhoto('taco veggie', { use: 'card' });
    assert.equal(v.source, 'commons');
    assert.equal(v.id, 'commons:11');
    assert.match(v.credit, /Veggie tacos by Ana Ruiz, CC BY-SA 4\.0/);
    // No Commons photo of a lavender candle: the built-in candle, before
    // any photo with a candle somewhere in it.
    assert.equal((await findPhoto('candle lavender', { use: 'card' })).source, 'builtin');
    // With free, a photo that asks for a credit is passed over.
    assert.notEqual((await findPhoto('taco veggie', { use: 'card', free: true }))?.id, 'commons:11');
    // Off the web, the set alone.
    assert.equal(await findPhoto('snowmobile trail', { open: false }), null);
  } finally { web.restore(); }
});

test('placePhotos: searches in parallel, keeps the photos as app files, credits rows in the footer, draws what is not found', async () => {
  photoCache(await mkdtemp(path.join(tmpdir(), 'photos-')));
  const web = fakeWeb({
    'taco carnitas': [page(30, 'Carnitas tacos')],
    'taco nopal': [page(31, 'Nopal tacos', { license: 'CC0' })],
    'food truck': [page(32, 'Food truck at night')],
  });
  const saved = [];
  const save = async (bytes, meta) => { saved.push({ n: bytes.length, ...meta }); return `f-${meta.name}`; };
  try {
    const plan = { art: { accent: 'salsa red' }, views: [{ slug: 'home', screen: { page: 'front', body: [
      { hero: 'Tacos', photo: 'food truck', look: 'photo' },
      { media: ['Our story'], photo: 'xylophone quartet' },
      { band: ['Visit'], look: 'photo', photo: 'zeppelin hangar' },
      { footer: "Dave's", note: 'Open daily' },
    ] } }], seed: { tacos: [
      { name: 'Carnitas', image: 'photo:taco carnitas' },
      { name: 'Nopal', image: 'photo:taco nopal' },
    ], zorbs: [{ name: 'Mystery', image: 'photo:zorbing ball' }] } };
    const code = {};
    const out = await placePhotos(plan, code, { save });
    const body = plan.views[0].screen.body;
    assert.equal(body[0].src, 'f-commons-32.jpg', 'a Commons photo is stored with the app');
    assert.equal(body[0].credit, undefined, 'its credit is in the footer, not over the photo');
    assert.match(body[1].src, /^data:image\/svg\+xml,/, 'nothing found: a drawn picture');
    assert.ok(!decodeURIComponent(body[1].src).includes('<text'), 'never letters');
    assert.equal(body[2].src, undefined, 'a band with no photo is drawn on the design\'s gradient');
    assert.equal(plan.seed.tacos[0].image, 'f-commons-30.jpg');
    assert.equal(plan.seed.tacos[1].image, 'f-commons-31.jpg');
    assert.match(plan.seed.zorbs[0].image, /^data:image\/svg/, 'not a photo of another thing');
    // The row whose photo asks for a credit is credited in the footer; the
    // public-domain one is not.
    assert.match(body[3].note, /^Open daily · Photos: Food truck at night by Ana Ruiz, CC BY-SA 4\.0; Carnitas tacos by Ana Ruiz, CC BY-SA 4\.0 \(Wikimedia Commons\)$/);
    assert.match(code.home, /Carnitas tacos/);
    assert.equal(saved.length, 3);
    assert.match(out.sources, /3 from Wikimedia Commons/);
    // The bytes are cached: a second app downloads nothing again.
    const files = await readdir(path.join(photoCache(), 'img'));
    assert.equal(files.length, 3);
    const before = web.asked.length;
    const again = { views: [], seed: { tacos: [{ name: 'Carnitas', image: 'photo:taco carnitas' }] } };
    await placePhotos(again, {}, { save });
    assert.equal(web.asked.length, before, 'searches and bytes both from the cache');
  } finally { web.restore(); }
});

test('a row photo that needs a credit is not used when the app has no footer to give it', async () => {
  const web = fakeWeb({ 'taco birria': [page(40, 'Birria tacos')], taco: [page(41, 'Taco', { license: 'CC0' })] });
  try {
    const plan = { views: [], seed: { tacos: [{ name: 'Birria', image: 'photo:taco birria' }] } };
    await placePhotos(plan, {}, {});
    assert.ok(!String(plan.seed.tacos[0].image).includes('/40.jpg'));
  } finally { web.restore(); }
});

test('the drawn picture takes the accent and differs by subject', () => {
  assert.equal(accentHue('salsa red'), 4);
  assert.equal(accentHue('deep indigo'), 236);
  assert.notEqual(drawnPicture('mug', 'card', 'clay'), drawnPicture('bowl', 'card', 'clay'));
});

test('the footer keeps one credits line across versions', async () => {
  const web = fakeWeb({ 'taco suadero': [page(50, 'Suadero tacos')] });
  try {
    const plan = { views: [{ slug: 'home', screen: { page: 'front', body: [{ footer: 'D', note: 'Open daily · Photos: Old one by B, CC BY 2.0 (Wikimedia Commons)' }] } }],
      seed: { tacos: [{ name: 'Suadero', image: 'photo:taco suadero' }] } };
    await placePhotos(plan, {}, {});
    assert.equal(plan.views[0].screen.body[0].note, 'Open daily · Photos: Old one by B, CC BY 2.0; Suadero tacos by Ana Ruiz, CC BY-SA 4.0 (Wikimedia Commons)');
  } finally { web.restore(); }
});
