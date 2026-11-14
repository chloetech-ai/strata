// What people did in an app, for the studio's Activity view: a period's
// events, each with the name of the row it touched (the plant "Monstera", the
// check-in "Read 20 pages · 2026-11-08"), and the app's own words for its
// screens, tables and functions, so the view can tell it the way the app does.

import { physical, briefOf } from './store.mjs';
import { uxMeasures } from './measures.mjs';

// Columns that name a row, best first; then a column named after something
// else's name (plant_name); then the row it points to (plant, habit_id).
const NAMES = ['name', 'title', 'label', 'subject', 'headline', 'question', 'item', 'task', 'what', 'word', 'term', 'summary', 'text'];
const NAMED = /_(name|title|label)$/;
const DAY = /^\d{4}-\d{2}-\d{2}(?:[T ][\d:.]+Z?)?$/;
const MAX_EVENTS = 5000;

const clip = (s, n = 80) => (s.length > n ? `${s.slice(0, n - 1).trimEnd()}…` : s);
const str = (v) => (typeof v === 'string' && v.trim() && !/^(https?:|data:|\/api\/)/.test(v) ? v.trim().replace(/\s+/g, ' ') : null);

// plants -> plant, categories -> category, boxes -> box, people -> person.
export function singular(word) {
  const w = String(word || '').replace(/_/g, ' ');
  if (/people$/i.test(w)) return w.replace(/people$/i, 'person');
  if (/ies$/i.test(w)) return w.replace(/ies$/i, 'y');
  if (/(x|ch|sh|ss|us)es$/i.test(w)) return w.replace(/es$/i, '');
  if (/[^su]s$/i.test(w)) return w.slice(0, -1);
  return w;
}

// The table a column points to: plant or plant_id -> plants.
function refTable(col, tables, self) {
  const base = col.replace(/_id$/, '');
  if (base === 'id' || !base) return null;
  const names = [base, `${base}s`, `${base}es`, base.replace(/y$/, 'ies')];
  return tables.find((t) => t.name !== self && names.includes(t.name)) || null;
}

// A row's own name, when it has one.
export function labelOf(columns, row) {
  if (!row) return null;
  const cols = (columns || []).map((c) => c.name);
  for (const n of NAMES) if (cols.includes(n)) { const v = str(row[n]); if (v && !DAY.test(v)) return { text: clip(v), own: true }; }
  for (const c of cols) if (NAMED.test(c)) { const v = str(row[c]); if (v) return { text: clip(v), own: false }; }
  return null;
}

// The words for a row: its name, or the name of what it points to with its
// day ("Read 20 pages · 2026-11-08" for a habit's check-in), or its first
// short text. `cache` keeps rows already looked up, by table and id.
export async function rowLabel(db, appId, table, row, tables, cache = new Map()) {
  if (!row) return null;
  const spec = tables.find((t) => t.name === table);
  if (!spec) return null;
  let main = labelOf(spec.columns, row);
  const refs = new Set();
  if (!main) {
    for (const c of spec.columns || []) {
      const ref = refTable(c.name, tables, table);
      const id = str(row[c.name]);
      if (!ref || !id) continue;
      refs.add(c.name);
      const key = `${ref.name}/${id}`;
      if (!cache.has(key)) cache.set(key, (await db.get(physical(appId, ref.name), [id]).catch(() => []))[0] || null);
      const l = labelOf(ref.columns, cache.get(key));
      if (l) { main = { text: l.text, own: false }; break; }
    }
  }
  if (!main) {
    for (const c of spec.columns || []) {
      if (refs.has(c.name) || /_id$|^id$/.test(c.name) || !['text', 'keyword'].includes(c.type)) continue;
      const v = str(row[c.name]);
      if (v && !DAY.test(v) && v.length <= 80) { main = { text: clip(v), own: false }; break; }
    }
  }
  // A row named only by what it points to is told apart by its day.
  const day = main?.own ? null : (spec.columns || []).map((c) => row[c.name]).find((v) => typeof v === 'string' && DAY.test(v));
  const text = [main?.text, day ? day.slice(0, 10) : null].filter(Boolean).join(' · ');
  return text ? clip(text, 120) : null;
}

export function tablesOf(view) {
  return view.tables.map((t) => ({ name: t.spec.name, columns: t.spec.columns || [] }));
}

// The events of the last `days` days (all of them for 0), newest first, at
// most MAX_EVENTS, with the words for each row they touched: the label kept
// with the event (a removed row keeps its name), else the row as it is now.
export async function activity(engine, appId, { days = 30 } = {}) {
  const view = await engine.view(appId);
  const tables = tablesOf(view);
  const since = days > 0 ? new Date(Date.now() - days * 86400000).toISOString() : null;
  const events = await engine.store.events(appId, { since, limit: MAX_EVENTS });
  const want = new Map();
  for (const e of events) {
    if (!e.tbl || !e.rid || e.label || !tables.some((t) => t.name === e.tbl)) continue;
    if (!want.has(e.tbl)) want.set(e.tbl, new Set());
    want.get(e.tbl).add(e.rid);
  }
  const labels = new Map();
  const cache = new Map();
  for (const [tbl, ids] of want) {
    const list = [...ids].slice(0, 1000);
    const rows = await engine.db.get(physical(appId, tbl), list).catch(() => []);
    for (let i = 0; i < list.length; i++) {
      if (!rows[i]) continue;
      cache.set(`${tbl}/${list[i]}`, rows[i]);
      labels.set(`${tbl}/${list[i]}`, await rowLabel(engine.db, appId, tbl, rows[i], tables, cache));
    }
  }
  const counts = {};
  for (const t of view.tables) counts[t.spec.name] = await engine.store.countRows(physical(appId, t.spec.name)).catch(() => null);
  const screens = view.views.map((v) => ({ id: v.id, slug: v.slug, title: v.title, order: v.spec?.order ?? 50 })).sort((a, b) => a.order - b.order);
  return {
    app: { id: view.app.id, name: view.app.name, description: view.app.description || '', brief: briefOf(view.app) },
    days, since, now: new Date().toISOString(), truncated: events.length >= MAX_EVENTS,
    events: events.map((e) => {
      const out = {};
      for (const [k, v] of Object.entries(e)) if (v !== null && v !== undefined && v !== '') out[k] = v;
      if (!out.label && e.tbl && e.rid) { const l = labels.get(`${e.tbl}/${e.rid}`); if (l) out.label = l; }
      return out;
    }),
    screens,
    // How easy it is, by people's use: the owner's own tries (src "owner",
    // the studio and the command center) are measured apart.
    ux: uxMeasures(events.filter((e) => e.src !== 'owner'), { screens, brief: briefOf(view.app) }),
    uxAll: uxMeasures(events, { screens, brief: briefOf(view.app) }),
    tables: view.tables.map((t) => ({ name: t.spec.name, noun: singular(t.spec.name), rows: counts[t.spec.name] })),
    functions: view.functions.map((f) => ({ id: f.id, slug: f.slug, title: f.title || f.slug })),
  };
}
