// Three designs for each app: one quick call to a small model, given what the
// app is for, answers with three sets of design tokens (web/design.js). They
// are kept in Strata (sa_designs), and the owner picks one in the studio; a
// person using the app on its own can pick their own. Picking one swaps a
// block of CSS variables in each view: no code changes, nothing is rebuilt.
import { ask } from './llm.mjs';
import { CHOICES, checkDesign, presetDesigns, hslOf } from '../web/design.js';

export const DESIGN_MODEL = process.env.STRATA_APPS_DESIGN_MODEL || 'claude-haiku-5-5';

const choices = Object.entries(CHOICES).map(([k, opts]) => `- ${k}: ${Object.entries(opts).map(([v, what]) => `"${v}" (${what})`).join(', ')}`).join('\n');
// What tells two designs apart besides color (Doable: the three directions
// differ in structure, or people see one design three times).
export const SHAPE = ['look', 'titles', 'nav', 'density', 'corners', 'surface'];

export const DESIGN_SYSTEM = `You are a brand designer. You give a small app its look as a few tokens over the same screens: three designs that are three shapes of the app, not one shape in three colors.

Answer with a JSON array of exactly three designs and nothing else. Each design:
{"name": "one or two words", "about": "the mood in under 12 words", "scheme": "auto",
 "light": {"bg": "#rrggbb", "card": "#rrggbb", "fg": "#rrggbb", "accent": "#rrggbb"},
 "dark": {"bg": "#rrggbb", "card": "#rrggbb", "fg": "#rrggbb", "accent": "#rrggbb"},
 "look": "native", "font": "system", "titles": "system", "corners": "soft", "density": "regular", "surface": "grouped", "nav": "tabs", "width": "wide",
 "gradient": ["#rrggbb", "#rrggbb", "#rrggbb"], "motion": "calm"}

The tokens and their values:
${choices}

The accent is the app's identity: the one color for its buttons, links and the current selection, and nothing else is saturated. Choose its hue for this subject and the people who use it, as you would for a brand of its own. Green and teal are not a default: use them only when the subject itself is green (plants, gardens, farming, the outdoors, money), and never for more than one of the three.
Pages are white or genuinely dark: in light, card white and bg white or a faint cool gray; in dark, bg near black and card a little lighter. No beige, cream or pastel page wash. Text (fg) reads on bg and on card at 7:1 or more; the accent reads as text on card at 4.5:1 or more, in light and in dark (a lighter tint of it in dark). gradient is the accent and two neighbours that flatter it, for front pages: vivid, never neon.

The three differ in more than color: their accents come from three different hue families, and each pair differs in at least two of look, titles, nav, density, corners and surface. Each fits the app's subject and its people:
1. The natural one, look "native": the hue the subject suggests first (the art direction's accent and type when the app has one), regular density (compact when the app is mostly tables), tabs.
2. A bold one, look "bold": the key number in a color band and each row a card, a deep, strong accent of a different hue family, serif or rounded titles when they fit the subject.
3. A third shape: look "plain" when the app is mostly tables or records to compare (a compact console: outlined surface, square or soft corners, a sidebar when it has many views), otherwise "native" again with a dark scheme or an airy, editorial page and a third hue family.
Names are evocative and short (Linen, Harbor, Ember), never the app's name.`;

// What a palette must not be (Doable found models default every option to
// green; 7 of our 12 persona apps came out green or teal): a green accent
// where the subject is not green, or two of the three in one hue family.
// The model is asked once more with the problems named.
const GREEN_SUBJECT = /\b(plant|garden|farm|crop|seed|forest|tree|leaf|park|outdoor|hike|trail|eco|green|money|budget|finance|invest|savings|golf|tennis|herb|vegan|salad|matcha|tea)/i;
export function paletteProblems(designs, app) {
  const hues = designs.map((d) => hslOf(d.light?.accent));
  const isGreen = ([hh, sat]) => sat > 0.18 && hh >= 70 && hh <= 185;
  const subject = `${app?.name || ''} ${app?.description || ''}`;
  const out = [];
  const greens = hues.map((h, i) => (isGreen(h) ? i + 1 : 0)).filter(Boolean);
  if (greens.length && !GREEN_SUBJECT.test(subject)) out.push(`design${greens.length > 1 ? 's' : ''} ${greens.join(' and ')} use a green or teal accent, and this subject is not green: choose hues the subject suggests`);
  else if (greens.length > 1) out.push(`designs ${greens.join(' and ')} are both green or teal: only one may be`);
  const dist = (a, b) => { const d = Math.abs(a[0] - b[0]) % 360; return Math.min(d, 360 - d); };
  for (let i = 0; i < hues.length; i++) for (let j = i + 1; j < hues.length; j++) {
    if (hues[i][1] > 0.18 && hues[j][1] > 0.18 && dist(hues[i], hues[j]) < 28) out.push(`designs ${i + 1} and ${j + 1} have accents of one hue family: give them different hues`);
  }
  return out;
}

// The art direction the first plan writes (forge.mjs cleanArt): what the app
// should feel like, its photos, its accent and its titles' type.
export function designPrompt(app, view, direction = null) {
  const views = (view?.views || direction?.views || []).map((v) => v.title).filter(Boolean);
  const tables = (view?.tables?.map((t) => t.spec?.name) || direction?.tables || []).filter(Boolean);
  const art = direction?.art;
  return `The app: ${app.name}
What it is for: ${String(app.description || '').slice(0, 1200)}
${views.length ? `Its views: ${views.join(', ')}\n` : ''}${tables.length ? `Its tables: ${tables.join(', ')}\n` : ''}${art ? `Its art direction (design 1 follows it: its accent's hue, its titles' type, its mood; 2 and 3 are alternatives to it):
- concept: ${art.concept}
- mood: ${art.mood || '-'}
- layout: ${art.layout || '-'}
- photos: ${art.imagery || 'none'}
- accent: ${art.accent || '-'} (the main buttons' color: their words must read on it)
- titles: ${art.type || 'system'}
- main action: ${art.action || '-'}
` : ''}
Design three looks for it.`;
}

// Three shapes, made so in code rather than asked for (Doable's "structural
// fixes hold"): the first design has the native look (the studio's default),
// and when the three share one look the second is bold; a first design with
// an art direction takes its type. Returns pairs still too alike, in words.
export function shapeDesigns(designs, direction = null) {
  if (!designs.length) return [];
  designs[0].look = 'native';
  if (designs.length > 1 && designs.every((d) => d.look === designs[0].look)) designs[1].look = 'bold';
  const type = direction?.art?.type;
  if (type && CHOICES.titles[type]) designs[0].titles = type;
  // The art direction's layout sets the first design's width: a reading
  // column for an editorial page, the full width for a dashboard.
  const width = { editorial: 'narrow', tool: 'narrow', dashboard: 'wide', gallery: 'wide' }[direction?.art?.structure];
  if (width) designs[0].width = width;
  const out = [];
  for (let i = 0; i < designs.length; i++) for (let j = i + 1; j < designs.length; j++) {
    const differ = SHAPE.filter((k) => designs[i][k] !== designs[j][k]);
    if (differ.length < 2) out.push(`designs ${i + 1} and ${j + 1} differ only in ${differ.length ? differ[0] : 'color'}: make them differ in at least two of ${SHAPE.join(', ')}`);
  }
  return out;
}

const parse = (text) => {
  const m = String(text).match(/\[[\s\S]*\]/);
  const list = JSON.parse(m ? m[0] : text);
  if (!Array.isArray(list) || list.length < 3) throw new Error('the answer held fewer than three designs');
  return list.slice(0, 3).map((d) => checkDesign(d));
};

// Three designs for an app. A model that fails or answers nonsense gives way
// to three of the presets, so an app always has its three.
export async function makeDesigns(app, view, { ask: askFn = ask, model = DESIGN_MODEL, direction = null } = {}) {
  const t0 = Date.now();
  try {
    const prompt = designPrompt(app, view, direction);
    const r = await askFn({ system: DESIGN_SYSTEM, prompt, model, timeoutMs: 90_000, idleMs: 45_000 });
    let designs = parse(r.text);
    let cost = r.cost || 0;
    // A palette and shapes checked in code, not only asked for: one more try
    // when they miss, and the second answer is kept whatever it holds.
    const problems = [...paletteProblems(designs, app), ...shapeDesigns(designs, direction)];
    if (problems.length) {
      try {
        const again = await askFn({ system: DESIGN_SYSTEM, prompt: `${prompt}\n\nYour last answer:\n${JSON.stringify(designs.map(({ name, light, dark, ...rest }) => ({ name, light, dark, ...Object.fromEntries(SHAPE.map((k) => [k, rest[k]])) })))}\nWhat is wrong with it: ${problems.join('; ')}. Answer again with all three designs.`, model, timeoutMs: 90_000, idleMs: 45_000 });
        cost += again.cost || 0;
        designs = parse(again.text);
        shapeDesigns(designs, direction);
      } catch { /* the first answer stands */ }
    }
    return { designs, source: model, ms: Date.now() - t0, cost, problems, directed: !!direction?.art };
  } catch (e) {
    const designs = presetDesigns(app.id);
    shapeDesigns(designs, direction);
    return { designs, source: 'presets', ms: Date.now() - t0, cost: 0, error: e.message };
  }
}

// One generation per app at a time: a second ask waits for the first.
const making = new Map();
export function ensureDesigns(store, appId, { fresh = false, onMade = () => {}, make = makeDesigns, direction = null } = {}) {
  if (making.has(appId)) return making.get(appId);
  const p = (async () => {
    const had = await store.designs(appId);
    if (had && !fresh) return had;
    const app = await store.getApp(appId);
    if (!app) throw Object.assign(new Error(`no app ${appId}`), { status: 404 });
    const view = app.head ? await store.resolve(appId).catch(() => null) : null;
    const out = await make(app, view, direction ? { direction } : undefined);
    let designs = out.designs;
    let chosen = 0;
    // New alternatives keep the design in use where it is.
    if (had && fresh) {
      chosen = had.chosen;
      const others = out.designs.filter((d) => d.name !== had.designs[had.chosen]?.name);
      designs = had.designs.map((d, i) => (i === had.chosen ? d : others.shift() || d));
    }
    const saved = { designs, chosen, source: out.source };
    await store.putDesigns(appId, saved);
    onMade(saved, out);
    return saved;
  })().finally(() => making.delete(appId));
  making.set(appId, p);
  return p;
}
