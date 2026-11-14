// Photos for front pages. A screen names a photo by what it shows
// ({"hero": "...", "photo": "coffee shop interior"}), and when the version is
// stored each such block gets "src" (and "credit" when the photo's license
// asks for one); the query stays, so a later change can ask again. Example
// rows may hold "photo:a query" in a column, which becomes a photo likewise.
//
// A photo shows the thing itself: the query's subject is its first word that
// names a thing ("taco veggie", "candle lavender jar"), and every source must
// have it. Sources, in order:
// - Unsplash's API, when STRATA_APPS_UNSPLASH_KEY holds an access key (free
//   at unsplash.com/developers): any subject, credited as its guidelines ask;
// - the built-in set, library/photos.json: 304 Unsplash photos (free to use,
//   no credit needed), each checked by eye and described in words, when one
//   has the subject and the words that say which kind of it ("veggie" in a
//   veggie taco), not yet on the page;
// - Wikimedia Commons (no key; every file freely licensed): photos whose
//   title or description has the subject, credited with author and license,
//   downloaded once into a shared cache and stored with the app's files;
// - the built-in set again, a photo that has the subject but not all the
//   words, or that the page already shows;
// - a drawn picture: the design's colors in soft shapes, never letters.
// STRATA_APPS_PHOTOS=builtin keeps to the set (and the drawing).
//
// Searches run in parallel before photos are placed, each with a time limit,
// and their answers and the photos' bytes are cached on disk across builds.

import { readFileSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'library');
const UNSPLASH_KEY = process.env.STRATA_APPS_UNSPLASH_KEY || '';
const OPEN = process.env.STRATA_APPS_PHOTOS !== 'builtin';
// Wikimedia asks automated clients to say who they are.
const UA = 'chloedb-apps/0.1 (app builder; photo search)';

// How wide a photo is drawn, by where it goes (twice the CSS pixels).
export const WIDTHS = { hero: 2400, band: 2000, media: 1400, slide: 2000, card: 900, gallery: 1400, avatar: 240 };

const STOP = new Set('a an the of with and in on for at by to from photo photos image picture pictures shot view some our your my their its this that'.split(' '));
const IRREGULAR = { loaves: 'loaf', leaves: 'leaf', shelves: 'shelf', knives: 'knife', halves: 'half', wolves: 'wolf', scarves: 'scarf',
  children: 'child', people: 'person', men: 'man', women: 'woman', geese: 'goose', feet: 'foot', teeth: 'tooth', mice: 'mouse' };
const stem = (w) => (IRREGULAR[w] || w).replace(/ies$/, 'y').replace(/(?<=[a-z]{3})(es|s)$/, (m) => (m === 'es' ? 'e' : '')).replace(/e$/, '');
export const words = (text) => String(text || '').toLowerCase().split(/[^a-z0-9]+/).filter((w) => w && !STOP.has(w)).map(stem);

let SET = null;
export function builtinPhotos() {
  if (!SET) {
    let list = [];
    try { list = JSON.parse(readFileSync(path.join(DIR, 'photos.json'), 'utf8')); } catch { list = []; }
    SET = list.map((p) => ({ ...p, tagWords: new Set(words(p.tags)), aboutWords: new Set(words(p.about)) }));
  }
  return SET;
}

const unsplashUrl = (id, use) => `https://images.unsplash.com/photo-${id}?auto=format&fit=crop&q=80&w=${WIDTHS[use] || 1400}${use === 'avatar' ? `&h=${WIDTHS.avatar}&crop=faces` : ''}`;

// Words that say how a thing looks rather than what it is: "dark roast",
// "minimal desk", "close-up of latte art". They help pick between photos but
// never make one fit on their own (a dark abstract is not a dark roast).
const MOD = new Set(words(`dark light bright fresh modern happy beautiful cozy cosy minimal minimalist
  elegant premium luxury luxurious new old vintage rustic small large big close up closeup top overhead aerial
  colorful colourful warm cool soft bold simple clean professional young smiling stylish organic natural
  local handmade artisan fine best classic golden sunny moody dramatic white black red blue green pink purple
  yellow orange brown grey gray single flat lay detail various assorted hero background banner nice cute
  lovely delicious tasty perfect vibrant calm peaceful serene quiet busy friendly wide angle portrait
  landscape square image style styled studio boutique charming historic traditional authentic gourmet homemade`));
// Words that name a thing too loosely to be a photo's subject ("a group of
// runners" is a photo of runners).
const LOOSE = new Set(words('group groups set sets bunch pair team crowd people person thing things kind type collection selection row rows lot lots plate'));
// After these, a query says where the thing is, not which thing it is
// ("tacos on a plate": the tacos fit without the plate).
const WHERE = /\b(on|in|at|under|over|near|beside|behind|along|around|inside|outside|against|across|through|by)\b/i;

// What a query asks for: its words, the words that name things, the subject
// (the first of them that is not a loose word) and the words before "on",
// "in"... that say which kind of the subject (all of them must fit).
export function queryOf(query) {
  const q = [...new Set(words(query))];
  const things = q.filter((w) => !MOD.has(w));
  const content = things.length ? things : q;
  const subject = content.find((w) => !LOOSE.has(w)) || content[0] || '';
  const head = String(query || '').split(WHERE)[0];
  // Words of one letter ("woman's") and of doing ("cutting", "pouring") say
  // nothing of which thing it is.
  const kind = [...new Set(words(head))].filter((w) => w.length > 1 && !/ing$/.test(w) && !MOD.has(w) && !LOOSE.has(w));
  return { all: q, content, subject, kind: kind.length ? kind : subject ? [subject] : [] };
}

// The built-in photos that fit a query best: each word of the query that a
// photo's tags hold counts 2, one its description holds 1.5 (a look word 0.5).
// A photo fits when it has the query's subject (its first word that names a
// thing: "yoga class", "dark roast coffee"), two of its words ("house blend
// coffee bag") or more than half of them, so "kayaking river" does not get a
// city on a river. A photo already on the page is passed over while another
// fits. With strict, a photo fits only with the subject and every word of
// its kind ("taco veggie" is not any taco).
export function matchBuiltin(query, { used = new Set(), limit = 1, strict = false, product = false } = {}) {
  const Q = queryOf(query);
  const q = Q.all;
  const content = Q.content;
  if (!content.length) return [];
  const scored = [];
  for (const p of builtinPhotos()) {
    let s = 0;
    let hits = 0;
    const has = (w) => p.tagWords.has(w) || p.aboutWords.has(w);
    for (const w of q) {
      const got = p.tagWords.has(w) ? 2 : p.aboutWords.has(w) ? 1.5 : 0;
      if (!got) continue;
      if (content.includes(w)) { s += got; hits++; } else s += 0.5;
    }
    const subject = has(content[0]) || (Q.subject !== content[0] && has(Q.subject));
    if (strict && !(has(Q.subject) && Q.kind.every(has))) continue;
    // A product: half the words of its kind at least ("bowl ceramic" is not
    // a salad bowl). A scene (a hero, a band) fits more loosely.
    if (product && Q.kind.filter(has).length * 2 < Q.kind.length) continue;
    if (subject || hits >= 2 || hits / content.length > 0.5) scored.push({ p, s: s + (subject ? 1 : 0), used: used.has(p.id) });
  }
  // Another photo stands in for one already on the page only when it fits
  // nearly as well ("coffee cups" should not become laptops beside a cup).
  const top = Math.max(0, ...scored.map((x) => x.s));
  for (let k = scored.length - 1; k >= 0; k--) if (scored[k].used || scored[k].s < top * 0.6) scored.splice(k, 1);
  // Ties go to a photo picked by the query, so two apps asking alike do not
  // always get the same one.
  const tie = (id) => { let h = 0; for (const ch of query + id) h = (h * 31 + ch.charCodeAt(0)) >>> 0; return h; };
  scored.sort((a, b) => b.s - a.s || tie(a.p.id) - tie(b.p.id));
  return scored.slice(0, limit).map((x) => x.p);
}

// ---- the network, with a cache in memory and (when given a folder) on disk

const memo = new Map();
let CACHE_DIR = process.env.STRATA_APPS_PHOTO_CACHE || null;
export function photoCache(dir) { if (dir) CACHE_DIR = dir; return CACHE_DIR; }
const hash = (s) => createHash('sha1').update(s).digest('hex').slice(0, 24);

async function getJson(url, headers = {}, timeout = 6000) {
  if (memo.has(url)) return memo.get(url);
  const file = CACHE_DIR ? path.join(CACHE_DIR, 'search', `${hash(url)}.json`) : null;
  const p = (async () => {
    if (file) { try { return JSON.parse(await readFile(file, 'utf8')); } catch { /* not cached */ } }
    const res = await fetch(url, { headers: { 'user-agent': UA, ...headers }, signal: AbortSignal.timeout(timeout) });
    if (!res.ok) throw new Error(`${new URL(url).host} answered ${res.status}`);
    const out = await res.json();
    if (file) { await mkdir(path.dirname(file), { recursive: true }).catch(() => {}); await writeFile(file, JSON.stringify(out)).catch(() => {}); }
    return out;
  })();
  memo.set(url, p);
  p.catch(() => memo.delete(url));
  return p;
}

// A photo's bytes, from the disk cache or the network.
export async function photoBytes(url, { timeout = 8000 } = {}) {
  const file = CACHE_DIR ? path.join(CACHE_DIR, 'img', hash(url)) : null;
  if (file) { try { return await readFile(file); } catch { /* not cached */ } }
  const res = await fetch(url, { headers: { 'user-agent': UA }, signal: AbortSignal.timeout(timeout) });
  if (!res.ok) throw new Error(`${new URL(url).host} answered ${res.status}`);
  const type = res.headers.get('content-type') || '';
  if (!/^image\//.test(type)) throw new Error(`not an image: ${type}`);
  const bytes = Buffer.from(await res.arrayBuffer());
  if (file) { await mkdir(path.dirname(file), { recursive: true }).catch(() => {}); await writeFile(file, bytes).catch(() => {}); }
  return bytes;
}

async function fromUnsplash(query, use, used) {
  const orientation = use === 'avatar' ? 'squarish' : use === 'card' ? 'portrait' : 'landscape';
  const d = await getJson(`https://api.unsplash.com/search/photos?query=${encodeURIComponent(query)}&per_page=10&content_filter=high&orientation=${orientation}`, { authorization: `Client-ID ${UNSPLASH_KEY}`, 'accept-version': 'v1' });
  const r = (d.results || []).find((x) => !used.has(x.id)) || (d.results || [])[0];
  if (!r) return null;
  // Unsplash asks that a use be reported to its download endpoint.
  fetch(r.links.download_location, { headers: { authorization: `Client-ID ${UNSPLASH_KEY}` }, signal: AbortSignal.timeout(6000) }).catch(() => {});
  const sep = r.urls.raw.includes('?') ? '&' : '?';
  return { id: r.id, src: `${r.urls.raw}${sep}auto=format&fit=crop&q=80&w=${WIDTHS[use] || 1400}${use === 'avatar' ? `&h=${WIDTHS.avatar}&crop=faces` : ''}`,
    credit: `Photo: ${r.user.name} on Unsplash`, href: `${r.user.links.html}?utm_source=strata_apps&utm_medium=referral`, source: 'unsplash' };
}

// ---- Wikimedia Commons

// Its thumbnails come in fixed widths; a hero takes the larger.
const COMMONS_WIDTH = { hero: 1920, band: 1920, slide: 1920, media: 1280, gallery: 1280, card: 960, avatar: 330 };
// Smallest original that is sharp where it goes.
const MIN_WIDTH = { hero: 1600, band: 1400, slide: 1400, media: 1000, gallery: 1000, card: 640, avatar: 300 };
// Files that are rarely a photo of a product or a place as it is today:
// finds catalogues, scans of old books, maps, diagrams, logos, archives.
const NOT_A_PHOTO = /FindID|geograph\.org|\bscan\b|\bpage\s*\d|\bp\.\s*\d|\bmap\b|diagram|\blogo\b|\bicon\b|poster|\bstamp\b|banknote|coin\b|\bcrop\b|\bsketch|drawing|engraving|lithograph|illustration|\b1[0-8]\d\d\b|\b19[0-5]\d\b|museum|exhibit|archive|herbarium|specimen|collection of|SLNSW|DPLA|\bMET\b|Walters|Smithsonian|Rijksmuseum|LACMA|Brooklyn|Cycladic|Etruscan|Ming|Qing|dynasty|antique|ancient|century|pigment|State Library|Fortepan|Bundesarchiv|Library of Congress|\bNARA\b|\bLOC\b|candelabr|church|cathedral|funeral|grave|memorial/i;
const LIVING = /\b(plants?|flora|flowers?|species|genus|taxa|fungi|insects?|birds?|mammals?|animals?|botanical|cultivars?)\b/i;
const LIVING_ASKED = /\b(plants?|flowers?|garden|bouquet|florist|tree|leaf|leaves|herbs?|bird|animal|pet|dog|cat|horse|fish|insect|bee|butterfly|farm)\b/i;
const plainText = (html) => String(html || '').replace(/<[^>]*>/g, ' ').replace(/&amp;/g, '&').replace(/&quot;/g, '"').replace(/&#0?39;/g, "'").replace(/&[a-z]+;/g, ' ').replace(/\s+/g, ' ').trim();

// The searches to try, the most precise first: every word naming a thing,
// then the kind, then the subject alone.
export function commonsQueries(query) {
  const Q = queryOf(query);
  const raw = String(query).toLowerCase().split(/[^a-z0-9]+/).filter((w) => w && !STOP.has(w) && !MOD.has(stem(w)) && !LOOSE.has(stem(w)));
  const subjectWord = raw.find((w) => stem(w) === Q.subject) || Q.subject;
  const kindWords = raw.filter((w) => Q.kind.includes(stem(w)));
  return [...new Set([raw.slice(0, 5).join(' '), kindWords.join(' '), subjectWord].filter(Boolean))];
}

// A Commons file as a candidate photo, or null when it does not fit: a JPEG
// big and wide enough for where it goes, whose title has the subject, with a license that says how to credit it.
export function commonsCandidate(page, query, use) {
  const ii = page && page.imageinfo && page.imageinfo[0];
  if (!ii || ii.mime !== 'image/jpeg') return null;
  const title = String(page.title || '').replace(/^File:/, '').replace(/\.[a-z]+$/i, '');
  if (NOT_A_PHOTO.test(title)) return null;
  const m = ii.extmetadata || {};
  const w = ii.width || 0;
  const h = ii.height || 1;
  if (w < (MIN_WIDTH[use] || 800)) return null;
  const wide = ['hero', 'band', 'slide', 'media'].includes(use);
  if (wide && w / h < 1.2) return null;
  if (!wide && (w / h > 2.2 || h / w > 1.8)) return null;
  const license = plainText(m.LicenseShortName?.value);
  if (!license || /no restrictions|fair use|non-?free/i.test(license)) return null;
  const desc = plainText(m.ImageDescription?.value).slice(0, 300);
  const Q = queryOf(query);
  // A plant, an animal or a place that shares the thing's name ("golden
  // candle", a shrub) is not the thing, unless living things are asked for.
  const cats = (page.categories || []).map((c) => String(c.title || '').replace(/^Category:/, '')).join(' | ');
  if ((LIVING.test(cats) || LIVING.test(title)) && !LIVING_ASKED.test(query)) return null;
  const tw = new Set(words(title));
  const dw = new Set(words(desc));
  const has = (x) => tw.has(x) || dw.has(x);
  if (!Q.subject || !tw.has(Q.subject)) return null;
  let score = 0;
  for (const x of Q.content) score += tw.has(x) ? 2 : dw.has(x) ? 1 : 0;
  if (Q.kind.filter(has).length * 2 < Q.kind.length) return null;
  const whole = Q.kind.every(has);
  if (whole) score += 2;
  if (/^(cc0|public domain|pd)/i.test(license)) score += 0.5;
  score -= (page.index || 0) * 0.15;
  const artist = plainText(m.Artist?.value).replace(/^\s*(photo|photograph)\s*(by|:)\s*/i, '').slice(0, 60);
  const free = /^(cc0|public domain|pd)/i.test(license) || m.AttributionRequired?.value === 'false';
  const credit = `${title.slice(0, 60)}${artist ? ` by ${artist}` : ''}, ${/^(cc0|pd)$/i.test(license) ? 'public domain' : license}, Wikimedia Commons`;
  return { id: `commons:${page.pageid}`, url: ii.thumburl || ii.url, score, whole, credit, href: ii.descriptionurl || null, free, source: 'commons' };
}

async function commonsSearch(q, use) {
  const width = COMMONS_WIDTH[use] || 1280;
  const url = `https://commons.wikimedia.org/w/api.php?action=query&format=json&generator=search&gsrnamespace=6&gsrlimit=20&gsrsearch=${encodeURIComponent(`${q} filemime:image/jpeg`)}`
    + `&prop=imageinfo|categories&clshow=!hidden&cllimit=max&iiprop=url|size|mime|extmetadata&iiurlwidth=${width}&iiextmetadatafilter=LicenseShortName|Artist|ImageDescription|AttributionRequired`;
  const d = await getJson(url, {}, 6000);
  return Object.values(d?.query?.pages || {});
}

// With whole, only a photo that has every word of the kind ("candle
// lavender": lavender too); without, one that has the subject.
async function fromCommons(query, use, used, { free = false, whole = false } = {}) {
  for (const q of commonsQueries(query)) {
    const pages = await commonsSearch(q, use);
    const best = pages.map((p) => commonsCandidate(p, query, use)).filter((c) => c && (!whole || c.whole) && !used.has(c.id) && (!free || c.free)).sort((a, b) => b.score - a.score)[0];
    if (best) return { id: best.id, src: best.url, credit: best.free ? null : best.credit, href: best.href, source: 'commons', remote: true, free: best.free };
  }
  return null;
}

// ---- a drawn picture, when no photo shows the thing

// The accent the art direction names, as a hue ("salsa red", "deep indigo").
const HUES = [[/red|salsa|tomato|chili|cherry|ruby|crimson/, 4], [/coral|salmon|peach/, 14], [/orange|amber|rust|terracotta|copper|clay|ember/, 24],
  [/gold|mustard|honey|saffron|yellow|ochre/, 42], [/olive|moss|sage/, 80], [/green|forest|leaf|mint|emerald|pine/, 140], [/teal|sea|jade/, 172],
  [/sky|cyan|azure|river/, 200], [/blue|navy|cobalt|denim|ocean/, 218], [/indigo|ink/, 236], [/violet|purple|plum|lavender|grape/, 270],
  [/pink|rose|blush|magenta|berry|fuchsia/, 330], [/brown|coffee|cocoa|walnut|oak|stone|sand|earth|taupe|umber/, 28]];
export function accentHue(accent) {
  const t = String(accent || '').toLowerCase();
  for (const [re, hue] of HUES) if (re.test(t)) return hue;
  return 220;
}

// Soft overlapping shapes in the accent's family, placed by the query, so
// cards side by side differ; no text.
export function drawnPicture(query, use = 'card', accent = '') {
  const hue = accentHue(accent);
  let seed = 0;
  for (const ch of String(query)) seed = (seed * 31 + ch.charCodeAt(0)) >>> 0;
  const rnd = () => { seed = (seed * 1103515245 + 12345) >>> 0; return (seed >>> 8) / 16777216; };
  const [w, h] = ['hero', 'band', 'slide', 'media'].includes(use) ? [1600, 900] : use === 'avatar' ? [400, 400] : [900, 1100];
  const hsl = (dh, s, l, a = 1) => `hsla(${(hue + dh + 360) % 360},${s}%,${l}%,${a})`;
  const shapes = [];
  for (let i = 0; i < 4; i++) {
    const r = (0.25 + rnd() * 0.35) * Math.max(w, h);
    shapes.push(`<circle cx="${Math.round(rnd() * w)}" cy="${Math.round(rnd() * h)}" r="${Math.round(r)}" fill="${hsl(rnd() * 40 - 20, 45 + rnd() * 20, 52 + rnd() * 20, 0.45)}"/>`);
  }
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="xMidYMid slice"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="${hsl(-12, 38, 90)}"/><stop offset="1" stop-color="${hsl(10, 42, 74)}"/></linearGradient><filter id="b"><feGaussianBlur stdDeviation="${Math.round(w / 14)}"/></filter></defs><rect width="100%" height="100%" fill="url(#g)"/><g filter="url(#b)">${shapes.join('')}</g></svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

// ---- finding one photo

// One photo for a query, or null. `used` holds the ids already on the page.
// With free, only a photo that needs no credit (an example row has no place
// for one).
export async function findPhoto(query, { use = 'card', used = new Set(), open = OPEN, unsplash = UNSPLASH_KEY, free = false, again = true } = {}) {
  const q = String(query || '').trim();
  if (!q) return null;
  if (unsplash && !free) {
    try { const r = await fromUnsplash(q, use, used); if (r) return r; } catch { /* the built-in set next */ }
  }
  const [p] = matchBuiltin(q, { used, strict: true });
  if (p) return { id: p.id, src: unsplashUrl(p.id, use), credit: null, href: null, source: 'builtin', tone: p.tone };
  const web = open && use !== 'avatar';
  if (web) {
    try { const r = await fromCommons(q, use, used, { free, whole: true }); if (r) return r; } catch { /* the built-in set next */ }
  }
  // A photo of the subject, not of its kind: the built-in set's first (its
  // photos are chosen by eye), then, for a scene, the web's. A product card
  // never takes a web photo that only shares its subject's word.
  const product = use === 'card';
  const [loose] = matchBuiltin(q, { used, product });
  if (loose) return { id: loose.id, src: unsplashUrl(loose.id, use), credit: null, href: null, source: 'builtin', tone: loose.tone };
  if (web && !product) {
    try { const r = await fromCommons(q, use, used, { free }); if (r) return r; } catch { /* none */ }
  }
  // Nothing new fits: the best photo again, even if the page has it.
  const [best] = again ? matchBuiltin(q, { product }) : [];
  return best ? { id: best.id, src: unsplashUrl(best.id, use), credit: null, href: null, source: 'builtin' } : null;
}

// Warms the caches for many queries at once (their searches run in
// parallel), so placing them one by one after waits on nothing.
export async function prefetchPhotos(jobs, { open = OPEN, limit = 8, deadline = 12000 } = {}) {
  if (!open) return;
  const queries = [];
  for (const { query, use } of jobs) {
    if (!query || use === 'avatar' || matchBuiltin(query, { strict: true }).length) continue;
    for (const q of commonsQueries(query)) queries.push([q, use]);
  }
  const seen = new Set();
  const todo = queries.filter(([q, use]) => { const k = `${q}|${COMMONS_WIDTH[use] || 1280}`; if (seen.has(k)) return false; seen.add(k); return true; });
  let next = 0;
  const work = Promise.all(Array.from({ length: Math.min(limit, todo.length) }, async () => {
    while (next < todo.length) { const [q, use] = todo[next++]; await commonsSearch(q, use).catch(() => null); }
  }));
  await Promise.race([work, new Promise((ok) => setTimeout(ok, deadline))]);
}

// The built-in photos (and the app's stored ones) a screen's code already
// shows (an edit of another screen passes them over while others fit).
export const photoIds = (code) => [...String(code || '').matchAll(/images\.unsplash\.com\/photo-([0-9]+-[0-9a-f]+)/g)].map((m) => m[1]);

// Where a photo goes, from the block that holds it.
const USE = { hero: 'hero', band: 'band', media: 'media', gallery: 'gallery', quotes: 'avatar', team: 'avatar', carousel: 'card', features: 'card', cards: 'card' };

// The objects of a screen that name a photo and have none yet, with where
// each goes, in page order.
function photoJobs(screen) {
  const jobs = [];
  const walk = (v, use) => {
    if (Array.isArray(v)) { for (const x of v) walk(x, use); return; }
    if (!v || typeof v !== 'object') return;
    const kind = Object.keys(USE).find((k) => v[k] !== undefined);
    // What this block holds is drawn as the block says (a carousel of
    // slides fills the width); a photo of the block itself, as its kind.
    const inner = kind ? (kind === 'carousel' && v.look === 'slides' ? 'slide' : USE[kind]) : use;
    const own = kind === 'hero' || kind === 'band' || kind === 'media' ? USE[kind] : use || inner || 'card';
    if (typeof v.photo === 'string' && v.photo.trim() && !v.photo.startsWith('=') && (v.src === undefined || v.src === null || v.src === '')) jobs.push({ obj: v, query: v.photo, use: own, kind });
    for (const x of Object.values(v)) if (x && typeof x === 'object') walk(x, inner);
  };
  walk(screen?.body || [], null);
  return jobs;
}

// A remote photo becomes one of the app's files when it can (save is given
// and the download works); else it is drawn from where it is.
async function keep(r, save) {
  if (!r || !r.remote || !save) return r;
  try {
    const bytes = await photoBytes(r.src);
    const id = await save(bytes, { name: `${String(r.id).replace(/[^a-z0-9]+/gi, '-')}.jpg`, type: 'image/jpeg' });
    if (id) return { ...r, src: id };
  } catch { /* drawn from where it is */ }
  return r;
}

// Fills "src" (and "credit") in every object of a screen that names a photo
// and has none yet. Returns the photos placed. A block nothing is found for
// is drawn: a hero or band on the design's gradient (the page's own), a
// card or photo with a drawn picture.
export async function placeScreenPhotos(screen, { used = new Set(), find = findPhoto, save = null, accent = '', drawn = true } = {}) {
  const placed = [];
  const jobs = photoJobs(screen);
  if (find === findPhoto) await prefetchPhotos(jobs);
  const found = [];
  for (const j of jobs) {
    const r = await find(j.query, { use: j.use, used });
    if (r) used.add(r.id);
    found.push(r);
  }
  const kept = await Promise.all(found.map((r) => keep(r, save)));
  jobs.forEach((j, i) => {
    const r = kept[i];
    const v = j.obj;
    if (r) {
      v.src = r.src;
      if (r.credit) v.credit = r.credit;
      if (r.href && r.credit) v.credit_url = r.href;
      placed.push({ query: j.query, source: r.source });
      return;
    }
    if (!drawn) return;
    // A hero or band keeps no src: the page draws the design's gradient.
    if (j.kind === 'hero' || j.kind === 'band') placed.push({ query: j.query, source: 'gradient' });
    else if (j.kind !== 'quotes' && j.kind !== 'team' && j.use !== 'avatar') {
      v.src = drawnPicture(j.query, j.use, accent);
      placed.push({ query: j.query, source: 'drawn' });
    }
  });
  return placed;
}

// Table names that say nothing of what their rows show.
const GENERIC = new Set(words(`products items listings entries records things posts goods inventory catalog catalogue offerings menu articles
  pieces piece works workshops classes sessions services events courses lessons plans packages options specials collections stock shop store`));

// What an example row's photo may be asked as: its query, else the row's
// kind ("category": "Bread"), else its name, and last what its table holds
// ("coffees"). Never a photo of something else: a sofa is no croissant.
function rowAsks(query, row, table) {
  const text = (re) => Object.entries(row).filter(([k, v]) => re.test(k) && typeof v === 'string' && !v.startsWith('photo:') && v.length <= 60 && !/^https?:/i.test(v)).map(([, v]) => v);
  const kindsOf = text(/categor|kind|type|group|section|collection/i).filter((q) => queryOf(q).content.length);
  const asks = [query, ...kindsOf, ...text(/^(name|title)$|_name$/i)].filter((q) => q && queryOf(q).content.length);
  const what = table.toLowerCase().split(/[^a-z0-9]+/).filter((w) => w && !GENERIC.has(stem(w))).join(' ');
  return { asks, what, kinds: queryOf(query).content.length ? kindsOf.length : Math.max(0, kindsOf.length - 1) };
}

// An example row's photo: its query, its kind, then its name, each found as
// any photo is (a photo already on the page before a photo of another
// thing: the croissant on the front page is the croissant on the menu too). Openly licensed photos that need a credit
// only when the app has a footer to give it in.
async function seedPhoto(query, row, table, used, { find, credits }) {
  const { asks, what, kinds } = rowAsks(query, row, table);
  const free = !credits;
  // A photo not yet on the page before one again: the query and the row's
  // kind found anywhere; its name ("Morning Linen") only in the built-in
  // set, as a name is rarely what the photo shows.
  for (const q of asks.slice(0, 1 + kinds)) { const r = await find(q, { use: 'card', used, free, again: false }); if (r) return r; }
  for (const q of asks.slice(1 + kinds)) { const [p] = matchBuiltin(q, { used, product: true, strict: true }); if (p) return { id: p.id, src: unsplashUrl(p.id, 'card'), source: 'builtin' }; }
  // The same photo twice beats a drawing: a candle is a candle.
  for (const q of asks.slice(0, 1 + kinds)) { const [p] = matchBuiltin(q); if (p) return { id: p.id, src: unsplashUrl(p.id, 'card'), source: 'builtin' }; }
  // What the table holds, only when it names a thing and a photo has all of it.
  const p = (what && matchBuiltin(what, { used, strict: true })[0]) || (what && matchBuiltin(what, { strict: true })[0]) || null;
  return p ? { id: p.id, src: unsplashUrl(p.id, 'card'), source: 'builtin' } : null;
}

// The front page's footer, where the credits of example rows' photos go.
const footerOf = (plan) => {
  for (const v of plan?.views || []) for (const b of Array.isArray(v?.screen?.body) ? v.screen.body : []) if (b && typeof b === 'object' && b.footer !== undefined) return { view: v, block: b };
  return null;
};

// Starts every search a plan's screens and example rows need, in parallel
// (a build calls it as soon as its plan is checked; placing them later waits
// only for what is still on its way).
export function warmPhotos(plan, { open = OPEN } = {}) {
  if (!open) return Promise.resolve();
  const jobs = (plan?.views || []).filter((v) => v?.screen && typeof v.screen === 'object').flatMap((v) => photoJobs(v.screen));
  for (const [table, rows] of Object.entries(plan?.seed || {})) {
    for (const row of Array.isArray(rows) ? rows : []) {
      for (const val of Object.values(row || {})) if (typeof val === 'string' && val.startsWith('photo:')) for (const q of rowAsks(val.slice(6), row, table).asks.slice(0, 1)) jobs.push({ query: q, use: 'card' });
    }
  }
  return prefetchPhotos(jobs, { open });
}

// Every screen view of a plan, and its example rows ("photo:..." values).
// Options: open (search the web), save(bytes, {name, type}) -> a file id
// (the app's files), accent (the art direction's), drawn (draw what is not
// found), find (stands in for findPhoto in tests).
export async function placePhotos(plan, code, opts = {}) {
  const { open = OPEN, save = null, accent = plan?.art?.accent || '', drawn = true } = opts;
  const find = opts.find || ((q, o) => findPhoto(q, { ...o, open }));
  const placed = [];
  const used = new Set();
  const screens = (plan?.views || []).filter((v) => v?.screen && typeof v.screen === 'object');
  // Every search first, in parallel.
  if (open && !opts.find) await warmPhotos(plan, { open });
  for (const v of screens) {
    const got = await placeScreenPhotos(v.screen, { used, find, save, accent, drawn });
    if (got.length) { placed.push(...got); code[v.slug] = JSON.stringify(v.screen, null, 1); }
  }
  const footer = footerOf(plan);
  const credits = [];
  // With a footer, the credits of the photos found now go in its small
  // print rather than over the photos.
  if (footer) {
    for (const v of screens) {
      let moved = false;
      const walk = (x) => {
        if (Array.isArray(x)) { x.forEach(walk); return; }
        if (!x || typeof x !== 'object') return;
        if (typeof x.credit === 'string' && x.credit.endsWith(', Wikimedia Commons') && typeof x.src === 'string') {
          credits.push(x.credit.replace(/, Wikimedia Commons$/, ''));
          delete x.credit;
          delete x.credit_url;
          moved = true;
        }
        Object.values(x).forEach((y) => { if (y && typeof y === 'object') walk(y); });
      };
      walk(v.screen.body);
      if (moved) code[v.slug] = JSON.stringify(v.screen, null, 1);
    }
  }
  const rowJobs = [];
  for (const [table, rows] of Object.entries(plan?.seed || {})) {
    for (const row of Array.isArray(rows) ? rows : []) {
      for (const [k, val] of Object.entries(row || {})) {
        if (typeof val !== 'string' || !val.startsWith('photo:')) continue;
        const r = await seedPhoto(val.slice(6), row, table, used, { find, credits: !!footer });
        if (r) used.add(r.id);
        rowJobs.push({ row, k, query: val.slice(6), r });
      }
    }
  }
  const kept = await Promise.all(rowJobs.map((j) => keep(j.r, save)));
  rowJobs.forEach((j, i) => {
    const r = kept[i];
    if (r) {
      j.row[j.k] = r.src;
      placed.push({ query: j.query, source: r.source });
      if (r.credit) credits.push(r.credit.replace(/, Wikimedia Commons$/, ''));
    } else if (drawn) {
      j.row[j.k] = drawnPicture(j.query, 'card', accent);
      placed.push({ query: j.query, source: 'drawn' });
    } else j.row[j.k] = null;
  });
  // The credits of the rows' photos, in the footer's small print.
  if (credits.length && footer) {
    // One credits line, kept across versions: the ones it had and the new.
    const had = String(footer.block.note || '').match(/(?:^| · )Photos: (.*) \(Wikimedia Commons\)$/);
    const rest = had ? String(footer.block.note).slice(0, had.index) : String(footer.block.note || '');
    const all = [...new Set([...(had ? had[1].split('; ') : []), ...credits])];
    const line = `Photos: ${all.join('; ')} (Wikimedia Commons)`;
    footer.block.note = rest ? `${rest} · ${line}` : line;
    code[footer.view.slug] = JSON.stringify(footer.view.screen, null, 1);
  }
  const by = {};
  for (const p of placed) by[p.source] = (by[p.source] || 0) + 1;
  const NAMES = { builtin: 'from the built-in set', unsplash: 'from Unsplash', commons: 'from Wikimedia Commons', drawn: 'drawn', gradient: 'on the gradient' };
  return { count: placed.length, sources: Object.entries(by).map(([s, n]) => `${n} ${NAMES[s] || `from ${s}`}`).join(', ') };
}
