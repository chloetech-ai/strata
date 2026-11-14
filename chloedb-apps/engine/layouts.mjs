// A page structure for every screen, not just the first. The model chooses
// one per screen (LAYOUT in a CREATE SCREEN statement, "layout" in a
// screen's JSON); a screen that gives none gets the one that fits what it is
// for, from its patterns and blocks:
// - a front page: the art direction's layout when it is one a front page
//   draws (hero, split, editorial, gallery), else what its hero says;
// - numbers over time (dashboard, trend, summary), or a row of big numbers
//   over charts: dashboard;
// - one thing to do (the daily check-off, flashcards, a quiz, a form alone):
//   tool;
// - one row and what goes with it (detail), lessons and articles: editorial;
// - photos or cards first (a gallery, a carousel, rows as cards with a
//   photo): gallery;
// - anything wider (a board, a calendar, a week, a plain list): none, the
//   kit's own page.

import { parseLayout, LAYOUT_NAMES } from './quality.mjs';

const FRONT = ['hero', 'split', 'editorial', 'gallery'];

const blocksOf = (screen) => {
  const out = [];
  const walk = (v) => {
    if (Array.isArray(v)) { v.forEach(walk); return; }
    if (!v || typeof v !== 'object') return;
    out.push(v);
    for (const [k, x] of Object.entries(v)) if (k !== 'data' && x && typeof x === 'object') walk(x);
  };
  walk(screen?.body || []);
  return out;
};

// The layout a screen should have when it names none, or null for the
// kit's own page. patterns: the patterns it was expanded from.
export function layoutFor(screen, { patterns = [], art = null } = {}) {
  if (!screen || typeof screen !== 'object') return null;
  const used = new Set(patterns);
  const body = Array.isArray(screen.body) ? screen.body : [];
  const all = blocksOf(screen);
  const has = (k) => all.some((b) => b[k] !== undefined);
  if (screen.page === 'front') {
    const chosen = parseLayout(art?.structure || art?.layout || '');
    if (chosen && FRONT.includes(chosen)) return chosen;
    const hero = body.find((b) => b && b.hero !== undefined);
    if (hero?.look === 'split') return 'split';
    if (hero?.look === 'plain') return 'editorial';
    if (!hero && (has('gallery') || has('carousel'))) return 'gallery';
    return 'hero';
  }
  if (['board', 'calendar', 'week'].some((p) => used.has(p))) return null;
  if (['dashboard', 'trend', 'summary'].some((p) => used.has(p))) return 'dashboard';
  if (['checks', 'flashcards', 'quiz'].some((p) => used.has(p))) return 'tool';
  if (used.has('detail')) return 'editorial';
  if (has('gallery') || has('carousel')) return 'gallery';
  const charts = all.filter((b) => typeof b.ui === 'string' && /chart|gauge|sparkline|heatmap|histogram|stackedBar|statTiles/i.test(b.ui)).length;
  const kpis = all.filter((b) => Array.isArray(b.kpis)).reduce((n, b) => n + b.kpis.length, 0);
  if (charts >= 2 || (charts && kpis >= 2)) return 'dashboard';
  const forms = all.filter((b) => b.form !== undefined).length;
  const lists = all.filter((b) => b.table !== undefined || b.each !== undefined || b.checks !== undefined).length;
  if (forms === 1 && !lists && !charts && body.length <= 4) return 'tool';
  // Lessons, articles, a menu: rows of words to read.
  const reading = all.filter((b) => b.each !== undefined && Array.isArray(b.body) && blocksOf({ body: b.body }).some((x) => x.text !== undefined || typeof x === 'string')).length;
  if (reading && !forms && !charts && !used.has('crud')) return 'editorial';
  return null;
}

// Gives every screen view of a plan that names no layout the one that fits
// it, and writes it into its statement too (LAYOUT, its second line), so the
// statement still makes the screen. Returns the layouts given, by slug.
export function assignLayouts(plan, art = null) {
  const given = {};
  for (const v of plan?.views || []) {
    const s = v?.screen;
    if (!s || typeof s !== 'object') continue;
    if (s.layout !== undefined) {
      const l = parseLayout(s.layout);
      if (l) s.layout = l; else delete s.layout;
      if (l) continue;
    }
    const l = layoutFor(s, { patterns: v.patterns || [], art });
    if (!l || !LAYOUT_NAMES.includes(l)) continue;
    s.layout = l;
    given[v.slug] = l;
    if (typeof v.statement === 'string' && !/^\s*LAYOUT\b/im.test(v.statement)) {
      const lines = v.statement.split('\n');
      // After the CREATE line, or on the PAGE line of a front page.
      const page = lines.findIndex((x) => /^\s*PAGE\s+[a-z]+\s*$/i.test(x));
      if (page >= 0) lines[page] = `${lines[page].trimEnd()} LAYOUT ${l}`;
      else lines.splice(1, 0, `LAYOUT ${l}`);
      v.statement = lines.join('\n');
    }
  }
  return given;
}
