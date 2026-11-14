// What a well-made app looks like, held as rules and checked as numbers.
//
// Three parts, after the lessons of other app builders' design pipelines:
// - the art direction a new app's plan writes before its screens (a
//   layout idea, a type pairing, a color, its photos and the one main
//   action), and the rules its screens keep, as short text the forge sees;
// - the page structures a screen can choose (LAYOUT in a CREATE SCREEN
//   statement, "layout" in a screen's JSON), drawn by the kit (web/kit.css,
//   web/front.css), so apps stop sharing one layout;
// - the checks that are numbers, not opinions: text contrast (4.5:1, 3:1 for
//   large text, buttons included), 44 px targets on a phone, no sideways
//   scroll, the main action on the first phone screen, no raw or placeholder
//   labels. The drawn-page check (look.mjs) runs them in the browser; when
//   no browser is installed, staticChecks runs what can be read from a
//   screen and its design.

import { designVars, contrast as hexContrast } from '../web/design.js';

// ---- page structures

// The layouts a screen can choose, and what each is for. The first word of
// each description is what the model is told it looks like.
export const LAYOUTS = {
  hero: 'a landing: a big photo or color band first with the headline and the one main action on it, the rest below',
  split: 'two columns on a computer: the main part (the list, the form, the words) wide on the left, a narrower column beside it; stacked on a phone, main part first',
  editorial: 'a narrow reading column with large serif-like titles and generous space, rows as a numbered or ruled list (a menu, lessons, articles, a story)',
  dashboard: 'the numbers that matter in a band at the top, then cards in a two or three column grid (status, charts, recent rows)',
  gallery: 'photos or cards first, in a tight grid of tiles (products, rooms, works, dishes)',
  tool: 'one focused tool in a centered panel: the input or the main action large and first, results under it (a calculator, a quick add, a check-in)',
};
export const LAYOUT_NAMES = Object.keys(LAYOUTS);
// The same, in a few words each, for the statement's reference.
export const LAYOUT_SHORT = { hero: 'a photo or color band with the headline and the main action, then the rest', split: 'main part wide on the left, a column beside it', editorial: 'a narrow reading column, rows as a ruled list', dashboard: 'numbers in a band, then a grid of cards', gallery: 'photos or cards first, in tiles', tool: 'one centered panel, the main input first' };
const LAYOUT_ALIASES = { landing: 'hero', photo: 'hero', columns: 'split', sidebar: 'split', list: 'editorial', article: 'editorial', magazine: 'editorial', menu: 'editorial', board: 'dashboard', stats: 'dashboard', grid: 'gallery', tiles: 'gallery', cards: 'gallery', single: 'tool', focus: 'tool', 'single-tool': 'tool', calculator: 'tool' };

// A layout's name from what a statement or a plan says ("Split", "single-tool",
// "a gallery of the dishes"), or null.
export function parseLayout(v) {
  if (typeof v !== 'string') return null;
  const t = v.trim().toLowerCase();
  if (!t) return null;
  if (LAYOUTS[t]) return t;
  if (LAYOUT_ALIASES[t]) return LAYOUT_ALIASES[t];
  const first = t.match(/[a-z][a-z-]*/g) || [];
  for (const w of first) { if (LAYOUTS[w]) return w; if (LAYOUT_ALIASES[w]) return LAYOUT_ALIASES[w]; }
  return null;
}

// ---- the rules, as the forge sees them

export const ART_DOC = `- art: the app's art direction, for a new app, decided with the brief and before the screens; the designs and screens follow it. Invent ONE direction for this product, never a generic one:
  "concept": one sentence from the subject's own world ("a chalkboard menu by the truck's window", "a quiet ledger for a corner bakery"); "mood": three words;
  "layout": the first screen's structure, one of ${LAYOUT_NAMES.join(', ')}, then a few words on why (a taco truck: "editorial, the menu as a chalkboard list"; a course: "split, lessons beside progress"); vary it by subject, not by habit;
  "type": the title face, "system", "serif" (editorial, food, books), "rounded" (friendly, families, learning) or "mono" (technical), with the pairing ("serif titles over plain text");
  "accent": the one color of the main buttons, named as the subject would ("salsa red", "deep indigo"), green or teal only when the subject is green; the text on it must read (white on a deep color, dark ink on a pale one);
  "imagery": what the photos show, the real product itself (the tacos on the plate, the class in session), never a related ingredient, plant or background, or "none" when photos do not help the job;
  "action": the one main action and its words, a concrete verb ("Order tacos", "Start lesson 1"): it is on the first phone screen and is the only filled button there.`;

export const TASTE_DOC = `## What every screen keeps (checked after the build, on a 390 px phone and a computer)
- Pages are white or genuinely dark; no beige or pastel wash, no emoji icons, no ornamental bars or borders on cards.
- Text reads at 4.5:1 against what is behind it (3:1 for large titles), button words included; white words only on a deep color or a dark photo.
- Every control and link people tap is at least 44 by 44 px on a phone; nothing scrolls sideways at 390 px.
- The main action (the art's "action") shows on the first phone screen, without scrolling: a short hero, never a marketing-height introduction.
- Labels say what a number is, in the subject's words ("4 groups meet this week", "Lessons done"), never a raw column or table name ("Groups 4", "lesson_count", "Item 1").
- Honest copy: say what it does for the people who use it; no "Welcome to X", "all-in-one", seamless, elevate or effortless; buttons are verbs in sentence case; nothing made up (no ratings, reviews, awards or numbers the request or the rows do not give).
- Photos show the real product of this business or the real activity; representative photos never claim to be its actual stock.`;

// ---- checks that run in the browser and in node

// The functions the checks share, written so that their source can be sent
// into a page (look.mjs injects it as window.__q) and used here as they are:
// nothing in them refers to anything outside them.
export function qualityKit() {
  const lin = (x) => { const s = x / 255; return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4; };
  const lum = (c) => 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
  // #rgb, #rrggbb, #rrggbbaa, rgb() and rgba() (the forms a computed style
  // gives); anything else is null.
  const parseColor = (v) => {
    if (v && typeof v === 'object') return v;
    const t = String(v || '').trim().toLowerCase();
    if (t === 'transparent') return { r: 0, g: 0, b: 0, a: 0 };
    let m = t.match(/^#([0-9a-f]{3,8})$/);
    if (m) {
      let x = m[1];
      if (x.length === 3 || x.length === 4) x = [...x].map((c) => c + c).join('');
      if (x.length !== 6 && x.length !== 8) return null;
      const n = (i) => parseInt(x.slice(i, i + 2), 16);
      return { r: n(0), g: n(2), b: n(4), a: x.length === 8 ? n(6) / 255 : 1 };
    }
    m = t.match(/^rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)(?:[\s,/]+([\d.]+%?))?\s*\)$/);
    if (m) {
      const a = m[4] === undefined ? 1 : m[4].endsWith('%') ? parseFloat(m[4]) / 100 : parseFloat(m[4]);
      return { r: +m[1], g: +m[2], b: +m[3], a };
    }
    return null;
  };
  // A color with alpha drawn over an opaque one.
  const over = (top, under) => {
    const a = top.a === undefined ? 1 : top.a;
    return { r: top.r * a + under.r * (1 - a), g: top.g * a + under.g * (1 - a), b: top.b * a + under.b * (1 - a), a: 1 };
  };
  // The WCAG contrast ratio of text (fg, may be translucent) on bg (opaque).
  const contrastRatio = (fg, bg) => {
    const b = parseColor(bg); const f = parseColor(fg);
    if (!b || !f) return null;
    const under = b.a === undefined || b.a >= 1 ? b : over(b, { r: 255, g: 255, b: 255, a: 1 });
    const x = lum(over(f, under)); const y = lum(under);
    return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
  };
  // Large text (WCAG): 24 px, or 18.66 px bold.
  const isLarge = (size, weight) => Number(size) >= 24 || (Number(size) >= 18.66 && Number(weight) >= 700);
  const neededContrast = (size, weight) => (isLarge(size, weight) ? 3 : 4.5);
  // The colors of a CSS gradient (as a computed style writes it), or [] for
  // a picture (url(...)) or nothing.
  const gradientColors = (img) => {
    const t = String(img || '');
    if (!/gradient\(/.test(t) || /url\(/.test(t)) return [];
    return t.match(/#[0-9a-f]{3,8}\b|rgba?\([^)]*\)|color\([^)]*\)|\b(?:transparent|white|black)\b/gi) || [];
  };
  // A target smaller than a finger: under min px either way (half a pixel
  // of rounding allowed).
  const smallTarget = (w, h, min = 44) => Number(w) < min - 0.5 || Number(h) < min - 0.5;
  // A label that is not words for people: a raw column or table name
  // (snake_case, camelCase, a table's name alone), a placeholder, or one
  // generic word before or after a bare number ("Groups 4", "Items: 12").
  const GENERIC = /^(items?|entries|entry|records?|rows?|data|values?|count|total|number|num|amount|groups?|lessons?|courses?|modules?|users?|students?|tasks?|things?|objects?|stuff|misc|other|field\d*|column\d*|label|title|name|text|status|type|category|stat|metric|kpi)$/i;
  const rawLabel = (label, { names = [], value = undefined } = {}) => {
    const t = String(label ?? '').trim();
    if (!t) return null;
    if (/^[a-z][a-z0-9]*(_[a-z0-9]+)+$/.test(t)) return 'a column name (snake_case)';
    if (/^[a-z]+[A-Z][A-Za-z0-9]*$/.test(t)) return 'a code name (camelCase)';
    if (/^(lorem|ipsum|placeholder|tbd|todo|xxx+|n\/a|label|title|heading|text here|item \d+|option \d+|field \d+|column \d+|untitled)$/i.test(t)) return 'a placeholder';
    const bare = t.replace(/[:\s]*\d[\d.,]*\s*$/, '').replace(/^\s*\d[\d.,]*[:\s]*/, '').trim();
    const withNumber = bare !== t || (value !== undefined && value !== null && /^\s*[\d.,]+\s*$/.test(String(value)));
    const word = bare.toLowerCase();
    const isName = names.some((n) => { const x = String(n).toLowerCase(); return x === word || `${x}s` === word || x === `${word}s` || x.replace(/_/g, ' ') === word; });
    if (withNumber && /^[A-Za-z]+$/.test(bare) && (GENERIC.test(bare) || isName)) return 'one generic word beside a number';
    if (isName && /^[a-z]/.test(t)) return "the table's name as it is";
    return null;
  };
  return { parseColor, over, contrastRatio, isLarge, neededContrast, gradientColors, smallTarget, rawLabel };
}
export const Q = qualityKit();
// The kit as source for a page: `window.__q = (...)()`.
export const QUALITY_SRC = `window.__q = (${qualityKit.toString()})();`;

// ---- checks of a screen and a design, without a browser

// Every label a screen shows: big numbers, columns, buttons, headings.
export function screenLabels(screen) {
  const out = [];
  const say = (text, where, value) => { if (typeof text === 'string' && text && !text.startsWith('=') && !text.includes('{{')) out.push({ text, where, value }); };
  const walk = (x, key) => {
    if (Array.isArray(x)) { x.forEach((y) => walk(y, key)); return; }
    if (!x || typeof x !== 'object') return;
    // a big number's label is always read beside its number
    if (key === 'kpis' || key === 'stats') say(x.label, key, typeof x.value === 'number' ? x.value : 0);
    if (key === 'columns' && x.key) say(x.label, 'column');
    if (typeof x.button === 'string') say(x.button, 'button');
    if (typeof x.h === 'string') say(x.h, 'heading');
    if (x.use && x.with) for (const k of ['title', 'label', 'noun']) say(x.with[k], `${x.use}.${k}`);
    for (const [k, v] of Object.entries(x)) if (v && typeof v === 'object' && !['data', 'let', 'state', 'code', 'fields'].includes(k)) walk(v, k);
  };
  walk(screen?.body, 'body');
  return out;
}

// The labels of a screen that are not words for people, as problems.
export function labelProblems(screen, names = []) {
  const out = [];
  for (const l of screenLabels(screen)) {
    const why = Q.rawLabel(l.text, { names, value: l.value });
    if (why) out.push(`the ${l.where === 'kpis' || l.where === 'stats' ? 'big number' : l.where} label ${JSON.stringify(l.text)} is ${why}: say what it is in the subject's words ("Lessons done", "Groups meeting this week")`);
  }
  return [...new Set(out)].slice(0, 4);
}

// The design's own pairs of text and surface, measured: the page's text,
// its secondary text, the accent as text on a card, and the words on a
// filled button. Returns problems in words.
export function designProblems(design) {
  if (!design) return [];
  const out = [];
  for (const which of ['light', 'dark']) {
    let v;
    try { v = designVars(design, which); } catch { continue; }
    const pairs = [['text', v['--fg'], v['--bg']], ['text on cards', v['--fg'], v['--card']], ['secondary text', v['--muted'], v['--card']], ['links', v['--accent'], v['--card']], ['button words', v['--accent-fg'], v['--accent']], ['bold buttons', v['--deep-fg'], v['--deep']]];
    for (const [what, fg, bg] of pairs) {
      if (!/^#[0-9a-f]{6}$/i.test(fg || '') || !/^#[0-9a-f]{6}$/i.test(bg || '')) continue;
      const r = hexContrast(fg, bg);
      if (r < 4.5) out.push(`the ${which} design's ${what} read at ${r.toFixed(1)}:1 (${fg} on ${bg}); they need 4.5:1`);
    }
  }
  return out;
}

// The main action of a screen: its first primary action, or none.
export function mainAction(screen) {
  let found = null;
  const walk = (x) => {
    if (found || !x || typeof x !== 'object') return;
    if (Array.isArray(x)) { x.forEach(walk); return; }
    if (x.hero !== undefined && (x.actions || x.action)) { const a = [].concat(x.actions || x.action)[0]; found = { kind: 'hero', label: typeof a === 'string' ? a : a?.label }; return; }
    if (x.use === 'crud' || x.use === 'checks' || x.use === 'calendar' || x.use === 'quiz' || x.use === 'flashcards') { found = { kind: x.use }; return; }
    if (x.form !== undefined) { found = { kind: 'form', label: x.submit }; return; }
    if (x.checks !== undefined) { found = { kind: 'checks' }; return; }
    if (typeof x.button === 'string' && /primary/.test(String(x.class || ''))) { found = { kind: 'button', label: x.button }; return; }
    for (const [k, v] of Object.entries(x)) if (v && typeof v === 'object' && !['data', 'let', 'state', 'code', 'fields'].includes(k)) walk(v);
  };
  walk(screen?.body);
  return found;
}

// The checks of an app's screens and design that need no browser: what the
// drawn-page check falls back to (look.mjs), in its shape.
export function staticChecks({ screens = [], design = null, names = [] }) {
  const found = [];
  for (const s of screens) {
    for (const p of labelProblems(s.screen, names)) found.push({ view: s.title || s.slug, device: 'phone', problem: p, severity: 'high', by: 'static' });
    if (s.screen?.layout && !parseLayout(s.screen.layout)) found.push({ view: s.title || s.slug, device: 'phone', problem: `the layout ${JSON.stringify(s.screen.layout)} is not one of ${LAYOUT_NAMES.join(', ')}`, severity: 'low', by: 'static' });
  }
  if (screens.length && !screens.some((s) => mainAction(s.screen))) found.push({ view: screens[0].title || screens[0].slug, device: 'phone', problem: 'no screen has a main action (a form, a hero action or a primary button)', fix: "give the first screen the art direction's main action", severity: 'low', by: 'static' });
  for (const p of designProblems(design)) found.push({ view: '', device: 'phone', problem: p, severity: 'high', by: 'static' });
  return found;
}
