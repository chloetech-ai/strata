// The Activity view: what people did in an app, told in the app's own words
// (its screens' titles, its tables' nouns, its rows' names) and drawn in its
// own design (the studio shows it inside the app's stage, under its colors).
// mountActivity(box, { appId, api, onClose, openScreen }) -> { refresh, destroy }

const SVGNS = 'http://www.w3.org/2000/svg';
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  if (props && typeof props === 'object' && !(props instanceof Node) && !Array.isArray(props)) {
    for (const [k, v] of Object.entries(props)) {
      if (v === undefined || v === null || v === false) continue;
      if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
      else if (k === 'class') el.className = v;
      else if (k === 'value' || k === 'disabled' || k === 'hidden') el[k] = v;
      else el.setAttribute(k, v === true ? '' : v);
    }
  } else if (props !== undefined) children.unshift(props);
  const add = (c) => {
    if (c === null || c === undefined || c === false) return;
    if (Array.isArray(c)) c.forEach(add);
    else el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  };
  children.forEach(add);
  return el;
}
function s(tag, attrs, ...children) {
  const el = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) if (v !== undefined && v !== null) el.setAttribute(k, v);
  for (const c of children.flat()) if (c !== null && c !== undefined) el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  return el;
}

// ---- small icons (24 px grid, drawn with the text color)

const ICONS = {
  eye: ['M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12z', { c: [12, 12, 3] }],
  plus: ['M12 5v14M5 12h14'],
  pencil: ['M4 20h4L19 9l-4-4L4 16v4zM13.5 6.5l4 4'],
  trash: ['M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13'],
  search: ['M20 20l-4.6-4.6', { c: [11, 11, 6] }],
  filter: ['M4 5h16l-6 8v6l-4-2v-4z'],
  play: ['M8 5.5v13l11-6.5z'],
  leave: ['M12 9v4M12 17h.01M10.3 3.9 2.4 17.5A2 2 0 0 0 4.1 20.5h15.8a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z'],
  error: ['M15 9l-6 6M9 9l6 6', { c: [12, 12, 9] }],
  undo: ['M9 14 4 9l5-5M4 9h10.5a5.5 5.5 0 0 1 0 11H11'],
  people: ['M16 20v-1.5a3.5 3.5 0 0 0-3.5-3.5h-5A3.5 3.5 0 0 0 4 18.5V20M20 20v-1.5a3.5 3.5 0 0 0-2.5-3.4M15.5 4.2a3.5 3.5 0 0 1 0 6.6', { c: [10, 7.5, 3.5] }],
  check: ['M5 12.5l4.5 4.5L19 7.5'],
  clock: ['M12 7v5l3 2', { c: [12, 12, 9] }],
  spark: ['M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5 18 18M6 18l2.5-2.5M15.5 8.5 18 6'],
  close: ['M6 6l12 12M18 6 6 18'],
  refresh: ['M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6'],
  tap: ['M9 11.5V5a1.5 1.5 0 0 1 3 0v5.5M12 10V8.5a1.5 1.5 0 0 1 3 0v2M15 10.5a1.5 1.5 0 0 1 3 0V15a6 6 0 0 1-6 6h-.6a6 6 0 0 1-4.7-2.3l-2.2-2.9a1.5 1.5 0 0 1 2.3-1.9L9 16'],
  home: ['M4 11 12 4l8 7M6 9.5V20h12V9.5M10 20v-5h4v5'],
};
function icon(name, size = 14) {
  const [d, more] = ICONS[name] || ICONS.spark;
  return s('svg', { viewBox: '0 0 24 24', width: size, height: size, fill: 'none', stroke: 'currentColor', 'stroke-width': 2, 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true', class: 'ico' },
    s('path', { d }), more?.c ? s('circle', { cx: more.c[0], cy: more.c[1], r: more.c[2] }) : null);
}

// ---- words

const MIN = 60000, HOUR = 3600000, DAY = 86400000;
const plural = (n, one, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;
const article = (noun) => `${/^[aeiou]/i.test(noun) && !/^(uni|use|eu|one)/i.test(noun) ? 'an' : 'a'} ${noun}`;
const humanize = (w) => String(w || '').replace(/[_-]+/g, ' ').trim();
const cap1 = (w) => (w ? w[0].toUpperCase() + w.slice(1) : w);
const dur = (ms) => {
  if (!ms || ms < 1000) return ms ? '1 s' : '';
  if (ms < MIN) return `${Math.round(ms / 1000)} s`;
  if (ms < HOUR) return `${Math.round(ms / MIN)} min`;
  const hrs = Math.floor(ms / HOUR), mins = Math.round((ms % HOUR) / MIN);
  return mins ? `${hrs} h ${mins} min` : `${hrs} h`;
};
const clock = (t) => new Date(t).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
const startOfDay = (t) => { const d = new Date(t); d.setHours(0, 0, 0, 0); return d.getTime(); };
function dayName(t) {
  const today = startOfDay(Date.now());
  const d = startOfDay(t);
  if (d === today) return 'Today';
  if (d === today - DAY) return 'Yesterday';
  return new Date(t).toLocaleDateString([], { weekday: 'short', month: 'short', day: 'numeric' });
}
function ago(t) {
  const sec = (Date.now() - t) / 1000;
  if (sec < 45) return 'just now';
  if (sec < 3600) return `${Math.round(sec / 60)} min ago`;
  if (sec < 86400) return `${Math.round(sec / 3600)} h ago`;
  return dayName(t);
}
// A label's ISO days read as days: "Read · 2026-11-08" -> "Read · Nov 8".
const pretty = (label) => String(label || '').replace(/\b(\d{4})-(\d{2})-(\d{2})(?:[T ][\d:.]+Z?)?\b/g, (m, y, mo, d) => new Date(Number(y), Number(mo) - 1, Number(d)).toLocaleDateString([], { month: 'short', day: 'numeric' }));
const compact = (n) => (n >= 10000 ? `${(n / 1000).toFixed(n >= 100000 ? 0 : 1)}K` : n.toLocaleString());

// ---- the model: events into sessions, people, screens and tables

const DONE = new Set(['insert', 'update', 'delete', 'call', 'action']);
const LEAVE_CAP = 30 * MIN; // a tab left open all night is not half a day of use
const GROUP = { open: 'view', leave: 'view', insert: 'change', update: 'change', delete: 'change', call: 'run', action: 'find', abandon: 'trouble', error: 'trouble' };

function model(d) {
  const events = [...(d.events || [])].map((e) => ({ ...e, t: new Date(e.ts).getTime() })).filter((e) => Number.isFinite(e.t)).sort((a, b) => a.t - b.t);
  const screens = new Map(d.screens.map((x) => [x.slug, x]));
  const fns = new Map();
  for (const f of d.functions || []) { fns.set(f.id, f); fns.set(f.slug, f); }
  const nouns = new Map(d.tables.map((t) => [t.name, t.noun || t.name]));
  const known = new Map((d.people || []).map((p) => [p.id, p.name]));
  const visitors = new Map();
  const whoKey = (e) => e.person || e.session || 'someone';
  const who = (key) => {
    if (key === d.me) return 'You';
    if (known.has(key)) return known.get(key) || 'Someone';
    if (key === 'someone') return 'Someone';
    if (!visitors.has(key)) visitors.set(key, `Visitor ${visitors.size + 1}`);
    return visitors.get(key);
  };
  // Sessions: a browser tab's events; events with no tab (a rule, a call
  // from outside) go by person, split where nothing happened for 30 minutes.
  const sessions = [];
  const open = new Map();
  for (const e of events) {
    const key = e.session ? `s:${e.session}` : `p:${whoKey(e)}`;
    let ss = open.get(key);
    if (!ss || (!e.session && e.t - ss.end > 30 * MIN)) {
      ss = { key, who: whoKey(e), events: [], start: e.t, end: e.t };
      open.set(key, ss);
      sessions.push(ss);
    }
    if (ss.who === 'someone' && e.person) ss.who = e.person;
    ss.events.push(e);
    ss.end = Math.max(ss.end, e.t + (e.kind === 'leave' ? 0 : 0));
  }
  for (const ss of sessions) {
    ss.name = who(ss.who);
    ss.time = ss.events.filter((e) => e.kind === 'leave').reduce((n, e) => n + Math.min(Number(e.ms) || 0, LEAVE_CAP), 0);
    ss.length = Math.max(ss.end - ss.start, ss.time);
    ss.path = [];
    for (const e of ss.events) if (e.kind === 'open' && ss.path[ss.path.length - 1] !== e.detail) ss.path.push(e.detail);
  }
  const people = new Set(sessions.map((x) => x.who).filter((w) => w !== 'someone'));
  return { d, events, sessions, screens, fns, nouns, who, people };
}

const screenTitle = (m, slug) => m.screens.get(slug)?.title || cap1(humanize(slug)) || 'a screen';
const nounOf = (m, tbl) => m.nouns.get(tbl) || humanize(tbl) || 'row';
const fnTitle = (m, e) => { const f = m.fns.get(e.cap) || m.fns.get(e.detail); return cap1(humanize(f?.title || f?.slug || e.detail || 'a function')); };
const isNothing = (e) => e.kind === 'action' && /^search\b/.test(e.detail || '') && Number(e.results) === 0;

// One event in the app's words: [icon, tone, words...].
function say(m, e) {
  const noun = nounOf(m, e.tbl);
  const name = e.label ? h('b', pretty(e.label)) : null;
  switch (e.kind) {
    case 'open': return ['eye', '', ['Looked at ', h('b', screenTitle(m, e.detail))]];
    case 'insert': return ['plus', 'add', name ? [`Added ${article(noun)} `, name] : [`Added ${article(noun)}`]];
    case 'update': {
      const f = String(e.fields || '').split(',').filter(Boolean).map(humanize);
      const what = f.length ? (f.length > 3 ? `${f.slice(0, 3).join(', ')} and ${f.length - 3} more` : f.join(', ').replace(/, ([^,]*)$/, ' and $1')) : `the ${noun}`;
      return ['pencil', 'change', [`Changed ${what}`, name ? [' on ', name] : f.length ? ` on ${article(noun)}` : '']];
    }
    case 'delete': return ['trash', 'remove', name ? [`Removed the ${noun} `, name] : [`Removed ${article(noun)}`]];
    case 'call': return ['play', '', ['Ran ', h('b', fnTitle(m, e))]];
    case 'abandon': {
      const [tbl, mode] = String(e.detail || '').split(' ');
      const n = nounOf(m, e.tbl || tbl);
      return ['leave', 'warn', [mode === 'edit' ? `Started changing ${article(n)} and left it unsaved` : `Started a new ${n} and left it unsaved`, e.ms ? ` after ${dur(Number(e.ms))}` : '']];
    }
    case 'error': return ['error', 'bad', ['Hit an error: ', h('span', { class: 'act-err' }, String(e.detail || '').slice(0, 160))]];
    case 'action': {
      const [verb, ...rest] = String(e.detail || '').split(' ');
      if (verb === 'search') {
        const q = e.q || rest.join(' ');
        const found = e.results === undefined ? '' : Number(e.results) === 0 ? 'found nothing' : `${plural(Number(e.results), 'found', 'found')}`.replace(/^(\d[\d,]*) found$/, '$1 found');
        return ['search', Number(e.results) === 0 ? 'warn' : '', ['Searched ', h('b', `“${q}”`), found ? h('span', { class: 'act-meta' }, ` · ${found}`) : '']];
      }
      if (verb === 'filter') {
        const [k, ...v] = String(e.q || rest.join(' ')).split('=');
        return ['filter', '', ['Showed only ', h('b', v.join('=') || k), v.length ? h('span', { class: 'act-meta' }, ` · ${humanize(k)}`) : '']];
      }
      return ['spark', '', [cap1(humanize(verb)), rest.length ? [' ', h('b', rest.join(' '))] : '']];
    }
    default: return ['spark', '', [cap1(humanize(e.kind)), e.detail ? ` ${e.detail}` : '']];
  }
}

// A session's steps: a screen opened and left is one step with its time.
function steps(ss) {
  const out = [];
  const pending = new Map();
  for (const e of ss.events) {
    if (e.kind === 'leave') {
      const st = pending.get(e.detail);
      if (st) { st.ms = Math.min(Number(e.ms) || 0, LEAVE_CAP); pending.delete(e.detail); }
      continue;
    }
    const st = { e, ms: null };
    if (e.kind === 'open') pending.set(e.detail, st);
    out.push(st);
  }
  return out;
}

// ---- buckets for the charts: hours for a day, days, or weeks for a long span

function buckets(m, days) {
  const now = Date.now();
  if (days === 1) {
    const end = new Date(now); end.setMinutes(0, 0, 0);
    const list = [];
    for (let i = 23; i >= 0; i--) { const t = end.getTime() - i * HOUR; list.push({ t, end: t + HOUR, label: new Date(t).toLocaleTimeString([], { hour: 'numeric' }) }); }
    return { list, unit: 'hour' };
  }
  const first = m.events.length ? m.events[0].t : now;
  const span = days || Math.max(7, Math.ceil((startOfDay(now) - startOfDay(first)) / DAY) + 1);
  const week = span > 120;
  const step = week ? 7 * DAY : DAY;
  const n = Math.ceil(span / (week ? 7 : 1));
  const today = startOfDay(now);
  const list = [];
  for (let i = n - 1; i >= 0; i--) {
    const t = today - i * step - (week ? new Date(today).getDay() * 0 : 0);
    list.push({ t, end: t + step, label: new Date(t).toLocaleDateString([], { month: 'short', day: 'numeric' }), weekday: new Date(t).toLocaleDateString([], { weekday: 'short' }) });
  }
  return { list, unit: week ? 'week' : 'day' };
}

const METRICS = {
  done: { label: 'Things done', of: (b) => b.done, say: (n) => plural(n, 'thing done', 'things done') },
  visits: { label: 'Visits', of: (b) => b.visits, say: (n) => plural(n, 'visit') },
  time: { label: 'Time', of: (b) => Math.round(b.time / MIN), say: (n) => `${n} min in the app` },
};

function fill(m, days) {
  const bk = buckets(m, days);
  for (const b of bk.list) Object.assign(b, { done: 0, visits: 0, time: 0, people: new Set(), kinds: {} });
  const find = (t) => { for (let i = bk.list.length - 1; i >= 0; i--) if (t >= bk.list[i].t && t < bk.list[i].end) return bk.list[i]; return null; };
  for (const e of m.events) {
    const b = find(e.t);
    if (!b) continue;
    if (DONE.has(e.kind)) { b.done++; b.kinds[e.kind] = (b.kinds[e.kind] || 0) + 1; }
    if (e.kind === 'leave') b.time += Math.min(Number(e.ms) || 0, LEAVE_CAP);
    b.people.add(e.person || e.session || 'someone');
  }
  for (const ss of m.sessions) { const b = find(ss.start); if (b) b.visits++; }
  return bk;
}

// Clean ticks: 0, then steps of 1, 2 or 5 times a power of ten.
function niceMax(v) {
  if (v <= 4) return Math.max(1, Math.ceil(v));
  const p = 10 ** Math.floor(Math.log10(v));
  for (const k of [1, 2, 2.5, 5, 10]) if (k * p >= v) return k * p;
  return 10 * p;
}

// ---- the tooltip, one for the view

function tipper(root) {
  const tip = h('div', { class: 'act-tip', role: 'tooltip', hidden: true });
  root.append(tip);
  return {
    show(ev, ...content) {
      tip.replaceChildren(...content);
      tip.hidden = false;
      const r = tip.getBoundingClientRect();
      const x = Math.min(window.innerWidth - r.width - 8, ev.clientX + 14);
      const y = ev.clientY + r.height + 22 > window.innerHeight ? ev.clientY - r.height - 12 : ev.clientY + 16;
      tip.style.left = `${Math.max(8, x)}px`;
      tip.style.top = `${Math.max(8, y)}px`;
    },
    hide() { tip.hidden = true; },
  };
}

// ---- charts

function columnChart(bk, metric, width, tip) {
  const H = 190, top = 14, bottom = 26, left = 34, right = 8;
  const W = Math.max(280, width);
  const vals = bk.list.map((b) => METRICS[metric].of(b));
  const max = niceMax(Math.max(...vals, 0));
  const plotH = H - top - bottom;
  const band = (W - left - right) / bk.list.length;
  const bw = Math.max(2, Math.min(24, band * 0.62));
  const y = (v) => top + plotH - (v / max) * plotH;
  const svg = s('svg', { class: 'act-chart', viewBox: `0 0 ${W} ${H}`, width: W, height: H, role: 'img', 'aria-label': `${METRICS[metric].label} per ${bk.unit}` });
  for (const v of [0, max / 2, max]) {
    svg.append(s('line', { x1: left, x2: W - right, y1: y(v), y2: y(v), class: 'grid' }));
    svg.append(s('text', { x: left - 8, y: y(v) + 4, 'text-anchor': 'end', class: 'tick' }, metric === 'time' ? `${compact(v)}m` : compact(v)));
  }
  const every = Math.max(1, Math.ceil(bk.list.length / Math.max(2, Math.floor((W - left) / 64))));
  bk.list.forEach((b, i) => {
    const x = left + i * band + band / 2;
    const v = vals[i];
    if (v > 0) {
      const y0 = y(0), y1 = Math.min(y0 - 2, y(v)), r = Math.min(4, bw / 2, (y0 - y1));
      svg.append(s('path', { class: `bar${i === bk.list.length - 1 ? ' now' : ''}`, d: `M${x - bw / 2},${y0}V${y1 + r}Q${x - bw / 2},${y1} ${x - bw / 2 + r},${y1}H${x + bw / 2 - r}Q${x + bw / 2},${y1} ${x + bw / 2},${y1 + r}V${y0}Z` }));
    }
    if (i % every === 0 || i === bk.list.length - 1) {
      const lab = i === bk.list.length - 1 ? (bk.unit === 'hour' ? 'Now' : bk.unit === 'day' ? 'Today' : 'This week') : bk.unit === 'day' && bk.list.length <= 8 ? b.weekday : b.label;
      if (i === bk.list.length - 1 || (bk.list.length - 1 - i) >= every * 0.6) svg.append(s('text', { x, y: H - 8, 'text-anchor': 'middle', class: 'tick' }, lab));
    }
    const hit = s('rect', { x: left + i * band, y: top, width: band, height: plotH, class: 'hit' });
    hit.addEventListener('mousemove', (ev) => {
      const when = bk.unit === 'hour' ? `${b.label}` : bk.unit === 'week' ? `Week of ${b.label}` : dayName(b.t);
      const kinds = Object.entries(b.kinds).map(([k, n]) => `${n} ${({ insert: 'added', update: 'changed', delete: 'removed', call: 'ran', action: 'searched or filtered' })[k] || k}`).join(' · ');
      tip.show(ev, h('b', when), h('div', METRICS[metric].say(vals[i])),
        h('div', { class: 'act-meta' }, [plural(b.visits, 'visit'), plural(b.people.size, 'person', 'people')].join(' · ')),
        kinds ? h('div', { class: 'act-meta' }, kinds) : null);
    });
    hit.addEventListener('mouseleave', () => tip.hide());
    svg.append(hit);
  });
  return svg;
}

// When in the week and the day people use it: 7 rows by 24 hours, darker
// for more (one hue, the app's accent).
function punchCard(m, width, tip) {
  const grid = Array.from({ length: 7 }, () => Array(24).fill(0));
  for (const e of m.events) {
    if (e.kind === 'leave') continue;
    const d = new Date(e.t);
    grid[(d.getDay() + 6) % 7][d.getHours()]++;
  }
  const max = Math.max(1, ...grid.flat());
  const left = 34, top = 4, W = Math.max(260, width);
  const cell = Math.max(6, Math.min(18, Math.floor((W - left) / 24) - 2));
  const gap = 2;
  const H = top + 7 * (cell + gap) + 20;
  const svg = s('svg', { class: 'act-punch', viewBox: `0 0 ${left + 24 * (cell + gap)} ${H}`, width: left + 24 * (cell + gap), height: H, role: 'img', 'aria-label': 'Activity by weekday and hour' });
  const days = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
  const long = ['Mondays', 'Tuesdays', 'Wednesdays', 'Thursdays', 'Fridays', 'Saturdays', 'Sundays'];
  days.forEach((name, r) => {
    svg.append(s('text', { x: left - 8, y: top + r * (cell + gap) + cell * 0.75, 'text-anchor': 'end', class: 'tick' }, name));
    for (let c = 0; c < 24; c++) {
      const n = grid[r][c];
      const rect = s('rect', { x: left + c * (cell + gap), y: top + r * (cell + gap), width: cell, height: cell, rx: Math.min(4, cell / 3), class: n ? 'on' : 'off' });
      if (n) rect.style.fillOpacity = String(0.18 + 0.82 * (n / max));
      rect.addEventListener('mousemove', (ev) => tip.show(ev, h('b', `${long[r]}, ${new Date(2020, 0, 1, c).toLocaleTimeString([], { hour: 'numeric' })}`), h('div', n ? plural(n, 'thing', 'things') + ' in this period' : 'Nothing yet')));
      rect.addEventListener('mouseleave', () => tip.hide());
      svg.append(rect);
    }
  });
  for (const c of [0, 6, 12, 18]) svg.append(s('text', { x: left + c * (cell + gap), y: H - 5, class: 'tick' }, new Date(2020, 0, 1, c).toLocaleTimeString([], { hour: 'numeric' })));
  // The busiest weekday and the busiest three hours of the day, in words.
  const byDay = grid.map((row) => row.reduce((a, b) => a + b, 0));
  const byHour = Array.from({ length: 24 }, (_, c) => grid.reduce((n, row) => n + row[c], 0));
  let best = 0, at = 0;
  for (let c = 0; c < 24; c++) { const n = byHour[c] + byHour[(c + 1) % 24] + byHour[(c + 2) % 24]; if (n > best) { best = n; at = c; } }
  const hour = (c) => new Date(2020, 0, 1, c % 24).toLocaleTimeString([], { hour: 'numeric' });
  const total = byHour.reduce((a, b) => a + b, 0);
  const day = byDay.indexOf(Math.max(...byDay));
  const scale = h('div', { class: 'act-scale', 'aria-hidden': 'true' }, 'Less', [0.18, 0.45, 0.72, 1].map((o) => h('i', { style: `opacity:${o}` })), 'More');
  return h('div', { class: 'act-punchwrap' }, svg, scale,
    total ? h('div', { class: 'act-busy' },
      h('div', h('span', { class: 'act-meta' }, 'Busiest day'), h('b', long[day])),
      h('div', h('span', { class: 'act-meta' }, 'Busiest hours'), h('b', `${hour(at)} to ${hour(at + 3)}`)),
      h('div', h('span', { class: 'act-meta' }, 'In those hours'), h('b', `${Math.round((best / total) * 100)}% of use`))) : null);
}

function sparkline(vals) {
  const W = 88, H = 26;
  if (vals.length < 2 || !vals.some((v) => v > 0)) return null;
  const max = Math.max(...vals, 1);
  const pts = vals.map((v, i) => [2 + (i / (vals.length - 1)) * (W - 6), H - 3 - (v / max) * (H - 7)]);
  const last = pts[pts.length - 1];
  return s('svg', { class: 'act-spark', viewBox: `0 0 ${W} ${H}`, width: W, height: H, 'aria-hidden': 'true' },
    s('polyline', { points: pts.map((p) => p.map((x) => x.toFixed(1)).join(',')).join(' ') }),
    s('circle', { cx: last[0], cy: last[1], r: 3 }));
}

// ---- where people go: the app's screens, how often each is opened and for
// how long, and the arcs of going from one straight to another

function flow(m, width, tip, openScreen) {
  const stats = new Map(m.d.screens.map((x) => [x.slug, { opens: 0, time: 0, leaves: 0, firsts: 0 }]));
  const moves = new Map();
  for (const ss of m.sessions) {
    let prev = null;
    let first = true;
    for (const e of ss.events) {
      const st = stats.get(e.detail);
      if (e.kind === 'open' && st) {
        st.opens++;
        if (first) { st.firsts++; first = false; }
        if (prev && prev !== e.detail) { const k = `${prev}>${e.detail}`; moves.set(k, (moves.get(k) || 0) + 1); }
        prev = e.detail;
      } else if (e.kind === 'leave' && st) { st.time += Math.min(Number(e.ms) || 0, LEAVE_CAP); st.leaves++; }
    }
  }
  const list = m.d.screens;
  const maxOpens = Math.max(1, ...[...stats.values()].map((x) => x.opens));
  const node = (x) => {
    const st = stats.get(x.slug);
    const avg = st.leaves ? st.time / st.leaves : 0;
    return h('button', { class: `act-node${st.opens ? '' : ' idle'}`, title: `Open ${x.title}`, onclick: () => openScreen?.(x.slug) },
      h('span', { class: 'nt' }, x.title),
      h('span', { class: 'nn' }, st.opens ? plural(st.opens, 'open') : 'not opened'),
      h('span', { class: 'nbar' }, h('span', { style: `width:${Math.round((st.opens / maxOpens) * 100)}%` })),
      h('span', { class: 'nm' }, [avg ? `${dur(avg)} a visit` : null, st.firsts ? `first for ${plural(st.firsts, 'visit')}` : null].filter(Boolean).join(' · ') || ' '));
  };
  // On a phone the screens stand in a column and the moves are listed.
  if (width < 520) {
    const top = [...moves.entries()].sort((a, b) => b[1] - a[1]).slice(0, 5);
    return h('div', { class: 'act-flow narrow' }, h('div', { class: 'act-nodes col' }, list.map(node)),
      top.length ? h('ul', { class: 'act-moves' }, top.map(([k, n]) => { const [a, b] = k.split('>'); return h('li', h('span', screenTitle(m, a)), h('span', { class: 'arrow' }, '→'), h('span', screenTitle(m, b)), h('span', { class: 'act-meta' }, plural(n, 'time'))); })) : null);
  }
  const col = Math.floor(Math.max(132, Math.min(280, (width - 8) / Math.max(1, list.length))));
  const W = col * list.length;
  const idx = new Map(list.map((x, i) => [x.slug, i]));
  const cx = (i) => i * col + col / 2;
  const maxMove = Math.max(1, ...moves.values());
  const arcs = [...moves.entries()].map(([k, n]) => { const [a, b] = k.split('>'); return { a, b, n }; }).filter((x) => idx.has(x.a) && idx.has(x.b));
  const fwd = arcs.filter((x) => idx.get(x.a) < idx.get(x.b));
  const back = arcs.filter((x) => idx.get(x.a) > idx.get(x.b));
  const tall = (set) => Math.max(18, ...set.map((x) => 14 + Math.abs(idx.get(x.b) - idx.get(x.a)) * 22));
  const top = tall(fwd), bot = tall(back);
  const labelled = new Set(arcs.sort((x, y) => y.n - x.n).slice(0, 5));
  const arcSvg = (set, height, up) => {
    const svg = s('svg', { class: 'act-arcs', viewBox: `0 0 ${W} ${height}`, width: W, height, 'aria-hidden': 'true' });
    for (const x of set) {
      const i = idx.get(x.a), j = idx.get(x.b);
      const x1 = cx(i) + (up ? 8 : -8), x2 = cx(j) + (up ? -8 : 8);
      const y0 = up ? height : 0;
      const lift = Math.min(height - 4, 12 + Math.abs(j - i) * 22);
      const yc = up ? height - lift : lift;
      const w = 1.5 + 4.5 * (x.n / maxMove);
      const path = s('path', { d: `M${x1},${y0} C${x1},${yc} ${x2},${yc} ${x2},${y0}`, class: 'arc', 'stroke-width': w.toFixed(1) });
      const head = s('path', { d: up ? `M${x2 - 4},${y0 - 7} L${x2},${y0 - 1} L${x2 + 4},${y0 - 7}` : `M${x2 - 4},${y0 + 7} L${x2},${y0 + 1} L${x2 + 4},${y0 + 7}`, class: 'arc-head' });
      const hit = s('path', { d: `M${x1},${y0} C${x1},${yc} ${x2},${yc} ${x2},${y0}`, class: 'arc-hit' });
      hit.addEventListener('mousemove', (ev) => tip.show(ev, h('b', `${screenTitle(m, x.a)} → ${screenTitle(m, x.b)}`), h('div', `straight from one to the other ${plural(x.n, 'time')}`)));
      hit.addEventListener('mouseleave', () => tip.hide());
      svg.append(path, head, hit);
      if (labelled.has(x)) {
        const mx = (x1 + x2) / 2, my = up ? height - lift * 0.75 : lift * 0.75;
        svg.append(s('rect', { x: mx - 11, y: my - 9, width: 22, height: 16, rx: 8, class: 'arc-pill' }), s('text', { x: mx, y: my + 3, 'text-anchor': 'middle', class: 'arc-n' }, String(x.n)));
      }
    }
    return svg;
  };
  const cards = h('div', { class: 'act-nodes', style: `grid-template-columns: repeat(${list.length}, ${col}px)` }, list.map(node));
  return h('div', { class: 'act-flow' }, h('div', { class: 'act-flow-in', style: `width:${W}px` }, fwd.length ? arcSvg(fwd, top, true) : null, cards, back.length ? arcSvg(back, bot, false) : null));
}

// ---- the sections

function tiles(m, bk) {
  const done = m.events.filter((e) => DONE.has(e.kind));
  const by = (k) => done.filter((e) => e.kind === k).length;
  const time = m.sessions.reduce((n, x) => n + x.time, 0);
  const searches = m.events.filter((e) => e.kind === 'action' && /^search\b/.test(e.detail || ''));
  const nothing = searches.filter((e) => Number(e.results) === 0).length;
  const tile = (ico, label, value, sub, spark, tone) => h('div', { class: `act-tile${tone ? ` ${tone}` : ''}` },
    h('div', { class: 'act-tl' }, icon(ico, 15), label), h('div', { class: 'act-tv' }, value), h('div', { class: 'act-ts' }, sub), spark);
  const parts = [[by('insert'), 'added'], [by('update'), 'changed'], [by('delete'), 'removed'], [by('call'), 'ran']].filter(([n]) => n).map(([n, w]) => `${n} ${w}`);
  return h('div', { class: 'act-tiles' },
    tile('people', 'People', compact(m.people.size || (m.sessions.length ? 1 : 0)), plural(m.sessions.length, 'visit'), sparkline(bk.list.map((b) => b.visits))),
    tile('check', 'Things done', compact(done.length), parts.join(' · ') || 'nothing added or changed', sparkline(bk.list.map((b) => b.done))),
    tile('clock', 'Time in the app', dur(time) || '0 min', m.sessions.length && time ? `${dur(time / m.sessions.length)} a visit` : 'no time measured yet', sparkline(bk.list.map((b) => b.time))),
    tile('search', 'Searches', compact(searches.length), searches.length ? (nothing ? h('span', { class: 'warn' }, icon('leave', 12), ` ${nothing} found nothing`) : 'every one found something') : 'nobody searched', null));
}

function changes(m) {
  const per = new Map();
  for (const e of m.events) {
    if (!['insert', 'update', 'delete'].includes(e.kind) || !e.tbl) continue;
    if (!per.has(e.tbl)) per.set(e.tbl, { insert: 0, update: 0, delete: 0, fields: new Map(), rows: new Map() });
    const t = per.get(e.tbl);
    t[e.kind]++;
    if (e.kind === 'update') for (const f of String(e.fields || '').split(',').filter(Boolean)) t.fields.set(f, (t.fields.get(f) || 0) + 1);
    if (e.rid) {
      const r = t.rows.get(e.rid) || { label: null, n: 0, last: 0, kinds: new Set() };
      r.label = e.label || r.label;
      r.n++; r.last = e.t; r.kinds.add(e.kind);
      t.rows.set(e.rid, r);
    }
  }
  if (!per.size) return h('div', { class: 'act-empty' }, 'Nothing was added, changed or removed in this period.');
  const tables = new Map(m.d.tables.map((t) => [t.name, t]));
  return h('div', { class: 'act-tables' }, [...per.entries()].sort((a, b) => (b[1].insert + b[1].update + b[1].delete) - (a[1].insert + a[1].update + a[1].delete)).map(([name, t]) => {
    const noun = nounOf(m, name);
    const rows = [...t.rows.values()].sort((a, b) => b.last - a.last).slice(0, 5);
    const stat = (ico, n, w, cls) => h('span', { class: `act-stat ${cls}${n ? '' : ' zero'}` }, icon(ico, 13), h('b', String(n)), ` ${w}`);
    const now = tables.get(name)?.rows;
    return h('div', { class: 'act-table' },
      h('div', { class: 'tt' }, h('b', cap1(humanize(name))), now !== null && now !== undefined ? h('span', { class: 'act-meta' }, plural(now, noun, humanize(name)) + ' now') : null),
      h('div', { class: 'act-stats' }, stat('plus', t.insert, 'added', 'add'), stat('pencil', t.update, 'changed', 'change'), stat('trash', t.delete, 'removed', 'remove')),
      t.fields.size ? h('div', { class: 'act-fields' }, [...t.fields.entries()].sort((a, b) => b[1] - a[1]).slice(0, 6).map(([f, n]) => h('span', { class: 'act-chip' }, humanize(f), h('i', `×${n}`)))) : null,
      h('ul', { class: 'act-rows' }, rows.map((r) => {
        const what = r.kinds.has('delete') ? (r.kinds.has('insert') ? 'added, then removed' : 'removed') : r.kinds.has('insert') ? (r.n > 1 ? `added, changed ${plural(r.n - 1, 'time')}` : 'added') : `changed ${plural(r.n, 'time')}`;
        return h('li', h('span', { class: `rn${r.kinds.has('delete') ? ' gone' : ''}` }, r.label ? pretty(r.label) : cap1(article(noun))), h('span', { class: 'act-meta' }, `${what} · ${ago(r.last)}`));
      })));
  }));
}

function attention(m) {
  const items = [];
  const nothing = new Map();
  for (const e of m.events) if (isNothing(e)) { const q = e.q || String(e.detail).slice(7); const x = nothing.get(q) || { n: 0, last: 0, screens: new Set(), people: new Set() }; x.n++; x.last = e.t; if (e.screen) x.screens.add(e.screen); x.people.add(e.person || e.session); nothing.set(q, x); }
  for (const [q, x] of nothing) items.push({ tone: 'warn', ico: 'search', n: x.n, last: x.last, words: [h('b', `“${q}”`), ` found nothing ${plural(x.n, 'time')}`, x.screens.size ? ` on ${[...x.screens].map((v) => screenTitle(m, v)).join(', ')}` : ''], hint: 'Something people expect to find is missing, or named differently.' });
  const gave = new Map();
  for (const e of m.events) if (e.kind === 'abandon') { const k = `${e.detail}|${e.screen || ''}`; const x = gave.get(k) || { e, n: 0, last: 0 }; x.n++; x.last = e.t; gave.set(k, x); }
  for (const x of gave.values()) {
    const [tbl, mode] = String(x.e.detail || '').split(' ');
    const n = nounOf(m, x.e.tbl || tbl);
    items.push({ tone: 'warn', ico: 'leave', n: x.n, last: x.last, words: [`${mode === 'edit' ? `Changes to ${article(n)}` : `A new ${n}`} left unsaved ${plural(x.n, 'time')}`, x.e.screen ? [' on ', h('b', screenTitle(m, x.e.screen))] : ''], hint: 'The form may ask for too much, or not say what it needs.' });
  }
  const added = new Map();
  for (const e of m.events) {
    if (e.kind === 'insert' && e.rid) added.set(`${e.tbl}/${e.rid}`, e);
    if (e.kind === 'delete' && e.rid) {
      const a = added.get(`${e.tbl}/${e.rid}`);
      if (a && e.t - a.t < 10 * MIN) items.push({ tone: 'warn', ico: 'undo', n: 1, last: e.t, words: [e.label || a.label ? h('b', pretty(e.label || a.label)) : cap1(article(nounOf(m, e.tbl))), ` was added and removed ${dur(e.t - a.t)} later`], hint: 'Adding by mistake is easy here, or what was added came out wrong.' });
    }
  }
  const errs = new Map();
  for (const e of m.events) if (e.kind === 'error') { const k = String(e.detail || '').slice(0, 120); const x = errs.get(k) || { n: 0, last: 0 }; x.n++; x.last = e.t; errs.set(k, x); }
  for (const [k, x] of errs) items.push({ tone: 'bad', ico: 'error', n: x.n, last: x.last, words: ['An error ', plural(x.n, 'time'), ': ', h('span', { class: 'act-err' }, k)] });
  items.sort((a, b) => (b.tone === 'bad') - (a.tone === 'bad') || b.n - a.n || b.last - a.last);
  const signals = (m.d.signals || []).filter((x) => x.kind !== 'nothing' && x.kind !== 'ux');
  // Each kind of problem says what it may mean once, on its first item.
  const said = new Set();
  for (const x of items) { if (x.hint && said.has(x.ico)) x.hint = null; else said.add(x.ico); }
  const out = [];
  out.push(items.length
    ? h('ul', { class: 'act-attn' }, items.slice(0, 8).map((x) => h('li', { class: x.tone },
      h('span', { class: 'ai' }, icon(x.ico, 15)), h('div', h('div', x.words), x.hint ? h('div', { class: 'act-meta' }, x.hint) : null), h('span', { class: 'act-meta when' }, ago(x.last)))))
    : h('div', { class: 'act-ok' }, icon('check', 15), ' Nothing looks wrong: no searches that found nothing, no forms left unsaved, no errors.'));
  if (signals.length) {
    out.push(h('div', { class: 'act-sub' }, 'What the engine noticed'));
    out.push(h('ul', { class: 'act-signals' }, signals.slice(0, 5).map((x) => h('li', icon('spark', 14), h('span', `People ${x.text}`), x.answered ? h('span', { class: 'act-chip' }, 'suggested') : null))));
  }
  return out;
}

// How easy it is: the UX measures from use (engine/measures.mjs), each
// against what the brief asks of the job, judged once there are enough
// visits, and how they moved from version to version.
function ease(m) {
  const ux = m.d.ux;
  if (!ux || !ux.sittings) return null;
  const M = ux.measures;
  const T = ux.targets;
  const pc = (x) => `${Math.round(x * 100)}%`;
  const one = (x) => String(Math.round(x * 10) / 10);
  const secs = (x) => (x < 90 ? `${Math.round(x)}\u00a0s` : dur(x * 1000));
  const acting = Math.round((M.acted || 0) * M.sittings);
  const first = m.screens.get(ux.first)?.title || null;
  const rows = [
    { k: 'taps', ico: 'tap', label: 'Taps to do something', n: M.tapsMeasured, val: M.tapsToAct, show: one, aim: `${T.taps} or fewer`, good: (x) => x <= T.taps, sub: 'from opening the app', unit: ['visit', 'visits'] },
    { k: 'secs', ico: 'clock', label: 'Time to do something', n: acting, val: M.secsToAct, show: secs, aim: `under ${T.secs}\u00a0s`, good: (x) => x <= T.secs, sub: 'from opening the app', unit: ['visit', 'visits'] },
    { k: 'first', ico: 'home', label: 'Done on the first screen', n: M.located, val: M.onFirst, show: pc, aim: `${pc(T.first)} or more`, good: (x) => x >= T.first, sub: first, unit: ['thing done', 'things done'] },
    { k: 'gaveUp', ico: 'pencil', label: 'Forms given up', n: M.forms, val: M.gaveUp, show: pc, aim: `${pc(T.gaveUp)} or fewer`, good: (x) => x <= T.gaveUp, sub: 'typed into, closed unsaved', unit: ['thing added', 'things added'] },
    { k: 'wrong', ico: 'trash', label: 'Added, then removed', n: M.inserts, val: M.wrong, show: pc, aim: `${pc(T.wrong)} or fewer`, good: (x) => x <= T.wrong, sub: 'within ten minutes', unit: ['row added', 'rows added'] },
    { k: 'undo', ico: 'undo', label: 'Removes undone', n: M.deletes, val: M.undone, show: pc, aim: `${pc(T.undo)} or fewer`, good: (x) => x <= T.undo, sub: 'Undo right after', unit: ['remove', 'removes'], least: 3 },
  ];
  const LEAST = 5;
  const cells = rows.map((r) => {
    const has = r.val !== null && r.val !== undefined;
    const judged = has && r.n >= (r.least || LEAST);
    const ok = judged && r.good(r.val);
    return h('div', { class: `ux-m${judged ? (ok ? ' ok' : ' over') : ' wait'}` },
      h('div', { class: 'ux-l' }, icon(r.ico, 14), r.label),
      h('div', { class: 'ux-v' }, has ? r.show(r.val) : '–'),
      h('div', { class: 'act-meta' }, r.sub || ''),
      h('div', { class: 'ux-s' }, judged
        ? [icon(ok ? 'check' : 'leave', 13), h('span', ok ? `On target: ${r.aim}` : `Over the aim: ${r.aim}`)]
        : h('span', has ? `${plural(r.n, ...r.unit)} so far; judged after ${r.least || LEAST}` : `Not measured yet: aim ${r.aim}`)));
  });
  const vs = (ux.versions || []).filter((x) => x.sittings && (x.tapsToAct !== null || x.secsToAct !== null)).slice(-4);
  const trend = vs.length > 1 ? h('div', { class: 'ux-vs' }, h('span', { class: 'act-meta' }, 'By version'),
    vs.map((x, i) => h('span', { class: `act-chip${i === vs.length - 1 ? ' on' : ''}` }, h('b', `v${x.v}`),
      [x.tapsToAct !== null ? ` ${one(x.tapsToAct)} taps` : '', x.secsToAct !== null ? ` · ${secs(x.secsToAct)}` : '', ` · ${plural(x.sittings, 'visit')}`].join('')))) : null;
  const found = (ux.findings || []).length ? h('ul', { class: 'act-attn ux-f' }, ux.findings.map((f) => h('li', { class: f.severity === 'high' ? 'bad' : '' },
    h('span', { class: 'ai' }, icon(f.key === 'taps' ? 'tap' : f.key === 'secs' ? 'clock' : f.key === 'first' ? 'home' : f.key === 'gaveUp' ? 'pencil' : f.key === 'wrong' ? 'trash' : f.key === 'undo' ? 'undo' : 'leave', 15)), h('div', h('div', f.text), h('div', { class: 'act-meta' }, 'The engine suggests a change for this from use.')), h('span')))) : null;
  return [h('div', { class: 'ux-grid' }, cells), trend, found];
}

const FILTERS = [['all', 'Everything'], ['change', 'Changes'], ['find', 'Searches'], ['trouble', 'Problems']];
const matches = (f, e) => f === 'all' || (f === 'trouble' ? GROUP[e.kind] === 'trouble' || isNothing(e) : GROUP[e.kind] === f);

function timeline(m, st, rerender) {
  let list = [...m.sessions].reverse();
  if (st.person !== 'all') list = list.filter((x) => x.who === st.person);
  if (st.filter !== 'all') list = list.filter((x) => x.events.some((e) => matches(st.filter, e)));
  const people = [...new Set(m.sessions.map((x) => x.who))];
  const bar = h('div', { class: 'act-bar' },
    h('div', { class: 'act-seg', role: 'tablist' }, FILTERS.map(([k, label]) => h('button', { class: st.filter === k ? 'on' : '', role: 'tab', 'aria-selected': String(st.filter === k), onclick: () => { st.filter = k; st.shown = 12; rerender(); } }, label))),
    people.length > 1 ? h('select', { class: 'act-select', 'aria-label': 'Whose visits', onchange: (e) => { st.person = e.target.value; st.shown = 12; rerender(); } },
      h('option', { value: 'all', selected: st.person === 'all' }, 'Everyone'), people.map((p) => h('option', { value: p, selected: st.person === p }, m.who(p)))) : null);
  if (!list.length) return [bar, h('div', { class: 'act-empty' }, 'No visits match.')];
  const cards = list.slice(0, st.shown).map((ss, i) => {
    const all = steps(ss);
    const shown = st.filter === 'all' ? all : all.filter((x) => matches(st.filter, x.e));
    const open = st.open.has(ss.key) || (!st.closed.has(ss.key) && i < 3);
    const counts = {};
    for (const e of ss.events) if (DONE.has(e.kind) || e.kind === 'abandon') counts[e.kind] = (counts[e.kind] || 0) + 1;
    const summary = [['insert', 'added'], ['update', 'changed'], ['delete', 'removed'], ['action', 'searched'], ['call', 'ran'], ['abandon', 'left unsaved']].filter(([k]) => counts[k]).map(([k, w]) => `${counts[k]} ${w}`).join(' · ');
    const initials = ss.name === 'You' ? 'You' : /^Visitor \d+$/.test(ss.name) ? `V${ss.name.slice(8)}` : ss.name.split(/\s+/).map((w) => w[0]).join('').slice(0, 2).toUpperCase();
    const toggle = () => { if (open) { st.open.delete(ss.key); st.closed.add(ss.key); } else { st.open.add(ss.key); st.closed.delete(ss.key); } rerender(); };
    return h('li', { class: `act-visit${open ? ' open' : ''}` },
      h('button', { class: 'vh', 'aria-expanded': String(open), onclick: toggle },
        h('span', { class: `av${ss.name === 'You' ? ' me' : ''}` }, initials),
        h('span', { class: 'vw' }, h('b', ss.name), h('span', { class: 'act-meta' }, `${dayName(ss.start)}, ${clock(ss.start)}${ss.length >= 1000 ? ` · ${dur(ss.length)}` : ''}`)),
        h('span', { class: 'vp' }, ss.path.slice(0, 6).map((slug, k) => [k ? h('span', { class: 'sep' }, '›') : null, h('span', { class: 'act-chip' }, screenTitle(m, slug))]), ss.path.length > 6 ? h('span', { class: 'act-meta' }, ` +${ss.path.length - 6}`) : null),
        h('span', { class: 'vs act-meta' }, summary || plural(all.length, 'step'))),
      open ? h('ol', { class: 'act-steps' }, shown.map(({ e, ms }) => {
        const [ico, tone, words] = say(m, e);
        return h('li', { class: tone }, h('span', { class: 'st' }, clock(e.t)), h('span', { class: 'si' }, icon(ico, 13)), h('span', { class: 'sw' }, words, ms ? h('span', { class: 'act-meta' }, ` · ${dur(ms)}`) : null));
      })) : null);
  });
  return [bar, h('ol', { class: 'act-visits' }, cards),
    list.length > st.shown ? h('button', { class: 'act-more', onclick: () => { st.shown += 20; rerender(); } }, `Show ${Math.min(20, list.length - st.shown)} more of ${plural(list.length - st.shown, 'visit')}`) : null];
}

// ---- the view

const RANGES = [[1, '24 hours'], [7, '7 days'], [30, '30 days'], [0, 'All']];

export function mountActivity(box, { appId, api, onClose, openScreen }) {
  const st = { days: 30, metric: 'done', filter: 'all', person: 'all', mine: false, shown: 12, open: new Set(), closed: new Set(), data: null, sig: '', at: 0, error: null, loading: true };
  try { const v = Number(sessionStorage.getItem(`sa-act-days-${appId}`)); if ([1, 7, 30, 0].includes(v) && sessionStorage.getItem(`sa-act-days-${appId}`) !== null) st.days = v; } catch {}
  const root = h('div', { class: 'act' });
  box.append(root);
  const tip = tipper(box);
  let timer = null;
  let gone = false;

  async function load(force) {
    try {
      const data = await api('GET', `/api/apps/${appId}/activity?days=${st.days}`);
      if (gone) return;
      const sig = `${data.events.length}|${data.events[0]?.ts || ''}|${data.signals?.length || 0}|${data.screens.map((x) => x.id).join(',')}`;
      st.error = null; st.loading = false; st.at = Date.now();
      if (!force && sig === st.sig) { stamp(); return; }
      st.sig = sig; st.data = data;
    } catch (e) { st.error = e.message; st.loading = false; }
    render();
  }

  function stamp() { const el = root.querySelector('.act-live span'); if (el) el.textContent = `Updated ${ago(st.at)}`; }

  function render() {
    tip.hide();
    const scroll = root.scrollTop;
    const width = Math.max(300, root.clientWidth - 48);
    // The owner's own tries of the app (src "owner": the studio, the command
    // center) are not people's use: left out unless asked for.
    const mine = (st.data?.events || []).filter((e) => e.src === 'owner').length;
    const mineBtn = mine ? h('button', { class: 'act-mine', 'aria-pressed': String(st.mine), title: st.mine ? 'Leave out what you did yourself in the studio and the command center' : 'Show what you did yourself in the studio and the command center',
      onclick: () => { st.mine = !st.mine; st.shown = 12; st.person = 'all'; render(); } }, st.mine ? `Your ${plural(mine, 'event')} shown · Hide` : `Your ${plural(mine, 'event')} left out · Show`) : null;
    const head = h('header', { class: 'act-head' },
      h('div', { class: 'act-title' },
        h('h1', 'Activity'),
        h('p', st.data ? ['What people did in ', h('b', st.data.app.name), st.data.truncated ? ' (the newest 5,000 events)' : ''] : 'What people did in this app'),
        st.data?.app.brief ? h('p', { class: 'act-brief' }, `Made for ${st.data.app.brief.who}, to ${st.data.app.brief.job.replace(/^to /i, '')}, ${st.data.app.brief.often}, on ${st.data.app.brief.device === 'both' ? 'a phone and a computer' : `a ${st.data.app.brief.device === 'desktop' ? 'computer' : 'phone'}`}.`) : null,
        h('div', { class: 'act-live' }, h('i'), h('span', st.at ? `Updated ${ago(st.at)}` : 'Loading'))),
      h('div', { class: 'act-tools' },
        mineBtn,
        h('div', { class: 'act-seg', role: 'tablist', 'aria-label': 'Period' }, RANGES.map(([d, label]) => h('button', { class: st.days === d ? 'on' : '', role: 'tab', 'aria-selected': String(st.days === d), onclick: () => { st.days = d; st.shown = 12; try { sessionStorage.setItem(`sa-act-days-${appId}`, String(d)); } catch {} load(true); } }, label))),
        h('button', { class: 'act-btn', title: 'Refresh', 'aria-label': 'Refresh', onclick: () => load(true) }, icon('refresh', 15)),
        onClose ? h('button', { class: 'act-btn', title: 'Back to the app', 'aria-label': 'Back to the app', onclick: onClose }, icon('close', 15)) : null));
    if (st.error) { root.replaceChildren(head, h('div', { class: 'act-empty' }, `The activity could not be read: ${st.error}`)); return; }
    if (!st.data) { root.replaceChildren(head, h('div', { class: 'act-empty' }, h('span', { class: 'spin' }), ' Reading what people did...')); return; }
    const m = model(st.mine || !mine ? { ...st.data, ux: st.data.uxAll || st.data.ux } : { ...st.data, events: st.data.events.filter((e) => e.src !== 'owner') });
    if (!m.events.length) {
      root.replaceChildren(head, h('div', { class: 'act-hello' }, h('div', { class: 'act-hello-ico' }, icon('spark', 22)),
        h('h2', st.days ? 'Nothing happened in this period' : 'Nobody has used it yet'),
        mine ? h('p', `Only you have tried it (${plural(mine, 'event')}), in the studio or the command center.`) : null,
        h('p', `Open ${st.data.app.name}'s screens, add something or search: each shows up here as it happens, in the app's own words.`),
        st.days ? h('button', { class: 'act-more', onclick: () => { st.days = 0; load(true); } }, 'Show all time') : null));
      return;
    }
    const bk = fill(m, st.days);
    const rerender = () => render();
    const card = (title, sub, ...body) => h('section', { class: 'act-card' }, h('div', { class: 'act-ch' }, h('h2', title), sub ? h('span', { class: 'act-meta' }, sub) : null), ...body);
    const metricSeg = h('div', { class: 'act-seg small', role: 'tablist', 'aria-label': 'Measure' }, Object.entries(METRICS).map(([k, x]) => h('button', { class: st.metric === k ? 'on' : '', role: 'tab', 'aria-selected': String(st.metric === k), onclick: () => { st.metric = k; rerender(); } }, x.label)));
    const wide = width > 760;
    const chartW = wide ? Math.floor(width * 0.6) - 40 : width - 32;
    const easy = ease(m);
    root.replaceChildren(head,
      tiles(m, bk),
      easy ? card('How easy it is', `${plural(m.d.ux.sittings, 'visit')}, against what it is for`, ...easy) : null,
      h('div', { class: `act-two${wide ? '' : ' one'}` },
        card(`${METRICS[st.metric].label} ${bk.unit === 'hour' ? 'by hour' : bk.unit === 'week' ? 'by week' : 'by day'}`, null, metricSeg, columnChart(bk, st.metric, chartW, tip)),
        card('When people use it', 'by weekday and hour', punchCard(m, wide ? Math.floor(width * 0.4) - 40 : width - 32, tip))),
      m.d.screens.length ? card('Where people go', 'each screen, and going straight from one to another', flow(m, width - 32, tip, openScreen)) : null,
      h('div', { class: `act-two even${wide ? '' : ' one'}` },
        card('What changed', null, changes(m)),
        card('Needs a look', null, ...attention(m))),
      card('Visits', `${plural(m.sessions.length, 'visit')}, newest first`, ...timeline(m, st, rerender)));
    root.scrollTop = scroll;
  }

  let lastW = 0;
  const ro = typeof ResizeObserver === 'function' ? new ResizeObserver(() => {
    const w = root.clientWidth;
    if (st.data && Math.abs(w - lastW) > 24) { lastW = w; render(); }
  }) : null;
  ro?.observe(root);
  render();
  load(true);
  timer = setInterval(() => { if (!document.hidden) load(false); }, 15000);
  return {
    refresh: () => { if (Date.now() - st.at > 4000) load(false); },
    destroy: () => { gone = true; clearInterval(timer); ro?.disconnect(); box.replaceChildren(); },
  };
}
