// The studio: start an app from a description, use it, ask for changes, take
// suggestions, go back to an earlier version, and read the code the engine
// generated and stored in Strata.

import { drawGraph, nodePanel, GRAPH_HELP } from './graph.js';
import { designCss, checkDesign, PRESETS } from './design.js';
import { mountActivity } from './activity.js';
import { frameCsp, FRAME_SANDBOX, linkToOpen } from './policy.js';

const $ = (sel, el = document) => el.querySelector(sel);
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

// One session per tab: what someone does in one sitting (a search, then the
// row they opened, then the form they gave up on) is told apart from the next.
const SESSION = (() => { try { const k = 'sa-session'; return sessionStorage.getItem(k) || (sessionStorage.setItem(k, Math.random().toString(36).slice(2, 12)), sessionStorage.getItem(k)); } catch { return Math.random().toString(36).slice(2, 12); } })();

// A call that failed: refused (a rule's refusal, or a visitor's access: 401,
// 403) or transient (nobody answered: the network, the engine restarting
// behind a proxy, too many requests), told apart from the app's own errors.
const TRANSIENT = new Set([408, 429, 502, 503, 504]);
const failed = (res, data) => Object.assign(new Error(data.error || res.statusText || `HTTP ${res.status}`),
  data.refused || res.status === 401 || res.status === 403 ? { refused: true } : {}, TRANSIENT.has(res.status) ? { transient: true } : {});
const unanswered = () => Object.assign(new Error('No answer from the app (the connection, or it is restarting)'), { transient: true });

// The company the app's person works in now (app.useCompany), sent with
// every call so rows scoped by :company_id are that company's.
const companyHeader = () => (S.company ? { 'x-sa-company': S.company } : {});
async function api(method, url, body, more) {
  const headers = { 'x-sa-session': SESSION, ...AS_STRANGER, ...companyHeader(), ...(more || {}) };
  if (body) headers['content-type'] = 'application/json';
  let res;
  try { res = await fetch(url, { method, headers, body: body ? JSON.stringify(body) : undefined }); } catch { throw unanswered(); }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw failed(res, data);
  return data;
}

const ago = (v) => {
  if (!v) return '';
  const s = (Date.now() - new Date(v).getTime()) / 1000;
  if (s < 45) return 'just now';
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
};

function toast(text) {
  const el = h('div', { class: 'sugg', style: 'position:fixed;left:50%;bottom:22px;transform:translateX(-50%);z-index:60;box-shadow:0 8px 24px rgba(0,0,0,.18);max-width:90vw' }, text);
  document.body.append(el);
  setTimeout(() => el.remove(), 3500);
}

const EXAMPLES = [
  'A reading list: I paste a link with a title and a note, tag it, and mark it read. Show what is unread first.',
  'A habit tracker: my daily habits, a check per day, and my current streak for each.',
  'A tiny CRM for my consulting: people, companies, and notes from each call, with a follow-up date.',
  'Track the books my kids read: who read it, when they finished, and a rating out of 5.',
  'A shared grocery list for our home, grouped by aisle, where ticking an item moves it to the bottom.',
];

// An app on its own, at /a/<id> or at / on a published app's own host (the
// engine names the app in a meta tag): its views, without the studio around them.
const BARE = (location.pathname.match(/^\/a\/([a-z0-9]+)/) || [])[1] || document.querySelector('meta[name="sa-app"]')?.content || null;
if (BARE) document.body.classList.add('bare');
// "See it as a stranger" (the app creator's Publish tab): the app's own page,
// every call made as a visitor would make it (x-sa-as), with the access the
// owner is weighing, whatever this browser's studio session.
const STRANGER = BARE ? (new URLSearchParams(location.search).get('stranger') || null) : null;
const AS_STRANGER = STRANGER ? { 'x-sa-as': 'visitor', 'x-sa-access': STRANGER } : {};
if (STRANGER) {
  addEventListener('DOMContentLoaded', () => {
    const said = { look: 'visitors can only look', add: 'visitors can look and add, and change only what they added', change: 'anyone can change anything' }[STRANGER] || '';
    const bar = document.createElement('div');
    bar.setAttribute('role', 'note');
    bar.style.cssText = 'position:sticky;top:0;z-index:50;padding:8px 14px;font:13px/1.4 system-ui,sans-serif;background:#fff4d6;color:#4a3500;border-bottom:1px solid #e8d38f';
    bar.textContent = `Seen as a stranger: no account, ${said}. What you do here changes the app's real data.`;
    document.body.prepend(bar);
  });
}

const S = {
  status: null,
  appId: null,
  app: null,
  view: null,
  frames: new Map(), // slug -> {iframe, capId, params}
  panel: 'change',
  errors: new Map(), // slug -> {message, repairing}
  lines: [],         // progress of the running request
  build: null,       // its steps, pieces and timing: {first, split, steps, pieces, elapsed, expect, at}
  runningId: null,
  ended: new Set(),  // requests whose done or failed came: a late read of the app's state does not bring them back
  logOpen: false,
  previews: new Map(), // slug -> {title, order, screen}: screens drawn while the model writes
  previewView: null,
  runningText: '',
  failed: null,
  suggesting: false,
  panelOpen: false,
  // A version the engine built from use, being tried (?v=N on the app's own
  // page): its views, functions and tables, on the app's real data.
  tryV: Number(new URLSearchParams(location.search).get('v')) || null,
};
const atV = () => (S.tryV ? `?v=${S.tryV}` : '');

let KIT = '';
let FRAME = '';
let SCREEN = '';
let FRONT = '';
async function assets() {
  if (!KIT) {
    const get = (u) => fetch(u).then((r) => r.text());
    const [kit, frame, screen, front, frontCss] = await Promise.all([get('/kit.css'), get('/frame.js'), get('/screen.js'), get('/front.js'), get('/front.css')]);
    // Front-page styles come after the kit's: a screen draws with both.
    [KIT, FRAME, SCREEN, FRONT] = [`${kit}\n${frontCss}`, frame, screen, front];
  }
}

// ---- live updates

function connect() {
  // An app on its own follows only that app (all the public may follow).
  const es = new EventSource(BARE ? `/api/stream?app=${BARE}` : '/api/stream');
  const dot = $('#status');
  es.addEventListener('hello', (e) => {
    const { feed } = JSON.parse(e.data);
    dot.className = 'status live';
    dot.title = feed ? 'Live: following ChloeDB\'s feed of changes' : 'Live';
  });
  es.onerror = () => { dot.className = 'status down'; dot.title = 'Not connected to the engine'; };
  const mine = (e) => { const d = JSON.parse(e.data); return d.app === S.appId ? d : null; };
  es.addEventListener('data', (e) => {
    const d = mine(e);
    if (!d) return;
    for (const f of S.frames.values()) f.iframe.contentWindow?.postMessage({ __sa: 1, kind: 'change', tables: d.tables }, '*');
    if (S.panel === 'inside') refreshApp();
  });
  es.addEventListener('app', (e) => { if (mine(e)) refreshApp(); else if (!S.appId) route(); });
  es.addEventListener('suggestions', (e) => { if (mine(e)) refreshApp(); });
  es.addEventListener('design', (e) => { if (mine(e)) refreshApp(); });
  es.addEventListener('candidates', (e) => { if (mine(e)) refreshApp(); });
  es.addEventListener('progress', (e) => {
    const d = mine(e);
    if (!d) return;
    if (S.runningText !== d.text) { S.runningText = d.text; S.lines = []; S.lastReplace = false; }
    if (d.replace && S.lastReplace) S.lines[S.lines.length - 1] = d.line;
    else S.lines.push(d.line);
    S.lastReplace = !!d.replace;
    S.failed = null;
    S.runningId = d.request;
    if (d.steps) S.build = { first: d.first, split: d.split, steps: d.steps, pieces: d.pieces || [], elapsed: d.elapsed || 0, expect: d.expect || {}, at: Date.now() };
    renderProgress();
  });
  es.addEventListener('preview', (e) => {
    const d = mine(e);
    if (!d) return;
    S.previews.set(d.slug, { slug: d.slug, title: d.title, order: d.order, screen: d.screen });
    if (!S.previewView || !S.previews.has(S.previewView)) S.previewView = d.slug;
    renderFrames();
  });
  es.addEventListener('done', (e) => {
    const d = mine(e);
    if (!d) return;
    S.ended.add(d.request);
    S.previews.clear();
    S.previewView = null;
    S.lines = [];
    S.build = null;
    S.runningText = '';
    toast(d.candidate ? `Built from your use, ready to try (version ${d.n})${d.summary ? `: ${d.summary}` : ''}` : `Version ${d.n} is live${d.summary ? `: ${d.summary}` : ''}`);
    refreshApp();
  });
  es.addEventListener('failed', (e) => {
    const d = mine(e);
    if (!d) return;
    S.ended.add(d.request);
    S.previews.clear();
    S.previewView = null;
    renderFrames();
    if (d.candidate) toast(`A change built from your use did not go through: ${d.error}`);
    else S.failed = { text: S.runningText, error: d.error, said: d.said, lines: S.lines.slice() };
    S.lines = [];
    S.build = null;
    S.runningText = '';
    refreshApp();
  });
  es.addEventListener('repair', (e) => {
    const d = mine(e);
    if (!d) return;
    toast('A capability failed; the engine is repairing it');
  });
  es.addEventListener('drawn', (e) => {
    const d = mine(e);
    if (!d) return;
    const graded = d.grade ? `, ${d.grade} of 5 for what it is for` : '';
    const ideas = d.suggested ? `; ${d.suggested} suggestion${d.suggested === 1 ? '' : 's'} under Suggested next` : '';
    toast(d.fixing ? `Checked the screens on a phone and a computer${graded}: ${d.high} thing${d.high === 1 ? '' : 's'} in the way; fixing ${d.high === 1 ? 'it' : 'them'} now${ideas}`
      : d.high ? `Checked the screens on a phone and a computer${graded}: ${d.high} thing${d.high === 1 ? '' : 's'} in the way${ideas}`
      : `Checked the screens on a phone and a computer${graded}: nothing in the way${ideas}`);
  });
}

// ---- routing

function route() {
  const parts = location.hash.replace(/^#\/?/, '').split('/').filter(Boolean);
  if (BARE) return openApp(BARE, parts[0] ? decodeURIComponent(parts[0]) : null);
  $('#nav-library').classList.toggle('on', parts[0] === 'library');
  if (parts[0] === 'app' && parts[1]) {
    const activity = parts[2] === '~activity';
    return openApp(parts[1], parts[2] && !activity ? decodeURIComponent(parts[2]) : null, activity);
  }
  leaveApp();
  if (parts[0] === 'library') return renderLibrary(parts[1] === 'graph' && parts[2] ? decodeURIComponent(parts[2]) : null);
  return renderHome();
}

function leaveApp() {
  leaveView(false);
  closeActivity();
  S.designCss = '';
  S.designKey = '';
  S.appId = null;
  S.app = null;
  S.frames.clear();
  S.errors.clear();
  S.lines = [];
  S.build = null;
  S.runningId = null;
  S.failed = null;
  S.runningText = '';
}

// ---- home

async function renderHome() {
  $('#crumbs').replaceChildren();
  const main = $('#main');
  const ta = h('textarea', { placeholder: 'Describe the first version of your app: what it keeps, what you do with it, what you want to see.', rows: 4 });
  const go = h('button', { class: 'btn primary', onclick: () => create() }, 'Build it');
  const err = h('span', { class: 'err small' });
  async function create() {
    const description = ta.value.trim();
    if (description.length < 5) { ta.focus(); return; }
    go.disabled = true;
    go.textContent = 'Starting...';
    try {
      const out = await api('POST', '/api/apps', { description });
      location.hash = `#/app/${out.app}`;
    } catch (e) { err.textContent = e.message; go.disabled = false; go.textContent = 'Build it'; }
  }
  ta.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) create(); });
  const list = h('div', { class: 'apps' }, h('div', { class: 'hint' }, 'Loading...'));
  main.replaceChildren(h('div', { class: 'page scroll' },
    h('section', { class: 'hero' },
      h('h1', 'Describe an app. Use it. Ask it to change.'),
      h('p', 'Each app starts from your description and grows by your requests. The engine writes the code, checks it, and stores every piece in ChloeDB, where other apps can reuse it. It watches what you do and suggests what to add next.')),
    h('section', { class: 'describe' }, ta, h('div', { class: 'bar' }, go, h('span', { class: 'hint' }, '⌘/Ctrl + Enter'), err)),
    h('div', { class: 'examples' }, EXAMPLES.map((x) => h('button', { class: 'example', onclick: () => { ta.value = x; ta.focus(); } }, x.split(':')[0]))),
    h('section', { class: 'col', style: 'display:flex;flex-direction:column;gap:10px' }, h('h2', { class: 'label' }, 'Your apps'), list),
    h('section', { class: 'how' },
      h('div', h('b', '1. Describe it'), 'The forge asks the model for tables, views and server functions, checks the code, and stores each piece as a row in ChloeDB.'),
      h('div', h('b', '2. Use it, change it'), 'Views run in sandboxed frames, functions in a WebAssembly sandbox. Ask for any change; each one is a new version you can go back from.'),
      h('div', h('b', '3. It grows'), 'What you do is recorded in ChloeDB. The engine suggests changes from it, and reuses what other apps built from the shared library.')),
  ));
  setTimeout(() => ta.focus(), 0);
  try {
    const { apps } = await api('GET', '/api/apps');
    list.replaceChildren(...(apps.length ? apps.map((a) => h('a', { class: 'appcard', href: `#/app/${a.id}` },
      h('div', { class: 'name' }, a.name),
      h('div', { class: 'desc' }, a.description),
      h('div', { class: 'meta' }, h('span', { class: 'pill' }, Number(a.head) ? `v${a.head}` : a.status === 'failed' ? 'did not build' : 'building'), h('span', ago(a.created)))))
      : [h('div', { class: 'hint' }, 'None yet. Describe one above, or pick an example.')]));
  } catch (e) { list.replaceChildren(h('div', { class: 'err' }, e.message)); }
}

// ---- library

// A graph question's answer: what it understood, then each piece with its
// app and, when found through others, the pieces between.
function answerOf(a) {
  const head = h('div', { class: 'answer-head' }, h('b', a.understood), h('span', { class: 'meta' },
    `${a.answer.length} found${a.found_by === 'meaning' ? ` · "${a.about.map((x) => x.label).join(', ')}" found by meaning` : ''} · from sa_graph in ChloeDB`));
  if (!a.answer.length) return [head, h('div', { class: 'hint' }, a.note || 'Nothing in any app does that yet.')];
  return [head, ...a.answer.map((r) => h('div', { class: 'libitem' },
    h('span', { class: `pill ${r.kind === 'function' && r.rule ? 'rule' : r.kind}` }, r.kind === 'view' ? 'screen' : r.kind === 'function' && r.rule ? 'rule' : r.kind),
    h('span', { class: 't' }, r.title), h('span', { class: 'meta' }, [r.kind === 'app' ? '' : r.app_name, r.via ? `through ${r.via.map((v) => v.label).join(' › ')}` : r.kind !== 'app' && r.hops > 0 && a.answer.some((x) => x.via) ? 'directly' : ''].filter(Boolean).join(' · ')),
    h('span', { style: 'margin-left:auto;display:flex;gap:6px' },
      r.kind === 'app' ? h('button', { class: 'btn small', onclick: () => { location.hash = `#/app/${r.app}`; } }, 'Open')
        : r.live ? h('button', { class: 'btn small', onclick: () => showCode(r.live) }, 'Code') : null,
      h('button', { class: 'btn small', title: 'what it uses, what depends on it, what goes with it', onclick: () => { location.hash = `#/library/graph/${encodeURIComponent(r.id)}`; } }, 'Links'))))];
}

async function renderLibrary(selected) {
  $('#crumbs').replaceChildren(h('span', { class: 'sep' }, '/'), h('b', 'Library'));
  const main = $('#main');
  const q = h('input', { class: 'search', placeholder: 'Search what every app has built, by words or meaning, or ask: which screens use the calendar?' });
  const out = h('div', { class: 'lib' });
  // Questions the graph answers, to click.
  const asks = h('div', { class: 'asks' }, h('span', { class: 'hint' }, 'Ask the graph:'),
    ['which screens use the calendar', 'which rules write votes', 'which apps use lib.money', 'what runs when an order is added', 'what does the cart screen use']
      .map((x) => h('button', { class: 'chip', onclick: () => { q.value = x; load().catch((e) => out.replaceChildren(h('div', { class: 'err' }, e.message))); } }, x)));
  const mode = location.hash.includes('graph') ? 'graph' : 'list';
  const switcher = h('div', { class: 'seg' },
    h('button', { class: mode === 'list' ? 'on' : '', onclick: () => { location.hash = '#/library'; } }, 'List'),
    h('button', { class: mode === 'graph' ? 'on' : '', onclick: () => { location.hash = '#/library/graph'; } }, 'Graph'));
  main.replaceChildren(h('div', { class: `page scroll${mode === 'graph' ? ' wide' : ''}` },
    h('section', { class: 'hero' }, h('h1', 'Library'), h('p', 'What every app runs now: shared components and helpers, which every app can call, screen patterns every app’s screens expand from, and each app’s views, functions and tables. Each version is a row of sa_capabilities in ChloeDB, searched by its words (BM25 over what it is and its code) and by its meaning (its title and description embedded), and linked to what it uses in a graph. The forge looks here before it writes something new.')),
    switcher, mode === 'graph' ? out : q, mode === 'graph' ? null : asks, mode === 'graph' ? null : out));
  if (mode === 'graph') return libraryGraph(out, { selected });
  let apps = [];
  try { ({ apps } = await api('GET', '/api/apps')); } catch { /* none */ }
  async function load() {
    const { capabilities, question } = await api('GET', `/api/library${q.value.trim() ? `?q=${encodeURIComponent(q.value.trim())}` : ''}`);
    if (question) return out.replaceChildren(...answerOf(question));
    const latest = new Map();
    for (const c of capabilities) {
      const k = `${c.app}/${c.kind}/${c.slug}`;
      if (!latest.has(k) || Number(latest.get(k).version) < Number(c.version)) latest.set(k, c);
    }
    // A search keeps its order, the best fit first; the whole list goes by kind.
    const items = [...latest.values()].filter((c) => q.value.trim() || c.kind !== 'table');
    if (!q.value.trim()) items.sort((a, b) => ({ component: 0, helper: 1, pattern: 2 }[a.kind] ?? 3) - ({ component: 0, helper: 1, pattern: 2 }[b.kind] ?? 3));
    const found = (c) => (c.found === 'meaning' ? ' · found by meaning' : c.found === 'both' ? ' · found by words and meaning' : '');
    out.replaceChildren(...(items.length ? items.map((c) => {
      const sel = h('select', { class: 'btn small' }, h('option', { value: '' }, 'Use in...'), apps.filter((a) => a.id !== c.app).map((a) => h('option', { value: a.id }, a.name)));
      sel.addEventListener('change', async () => {
        if (!sel.value) return;
        const target = sel.value;
        await api('POST', `/api/apps/${target}/requests`, { source: 'library', text: c.kind === 'component' || c.kind === 'helper'
          ? `Use the shared ${c.kind} ${c.slug} ("${c.title}") in this app, where it fits.`
          : c.kind === 'pattern' ? `Use the screen pattern ${c.slug} ("${c.title}") in this app, where it fits.`
          : `Bring in the ${c.kind} "${c.title}" from the app "${c.app_name}" (library id ${c.id}) and make it work with this app's data.` });
        location.hash = `#/app/${target}`;
      });
      return h('div', { class: 'libitem' },
        h('span', { class: `pill ${c.kind}` }, c.kind), h('span', { class: 't' }, c.title), h('span', { class: 'meta' }, c.kind === 'component' || c.kind === 'helper'
          ? `${c.kind === 'helper' ? 'lib' : 'ui'}.${c.slug} · v${c.version}${c.author === 'builtin' ? ' · built in' : ''} · ${c.used_by?.length ? `used by ${c.used_by.map((u) => `${u.name}${u.version < c.version ? ` (v${u.version})` : ''}`).join(', ')}` : 'not used yet'}${found(c)}`
          : `${c.app_name} · v${c.version}${c.author && c.author !== 'forge' ? ` · ${c.author}` : ''}${found(c)}`),
        h('span', { style: 'margin-left:auto;display:flex;gap:6px' }, h('button', { class: 'btn small', onclick: () => showCode(c.id) }, 'Code'),
          h('button', { class: 'btn small', title: 'what it uses, what depends on it, what goes with it', onclick: () => { location.hash = `#/library/graph/${encodeURIComponent(`${c.app}/${c.kind}/${c.slug}`)}`; } }, 'Links'), c.kind !== 'table' ? sel : null),
        c.description ? h('div', { class: 'd' }, c.description) : null);
    }) : [h('div', { class: 'hint' }, q.value ? 'Nothing matches.' : 'Nothing yet: build an app first.')]));
  }
  let t;
  q.addEventListener('input', () => { clearTimeout(t); t = setTimeout(load, 200); });
  load().catch((e) => out.replaceChildren(h('div', { class: 'err' }, e.message)));
}

// The library as a graph: every node of sa_graph, and a panel with what
// Strata's graph queries say about the one picked.
async function libraryGraph(out, { app, selected } = {}) {
  let nodes;
  try { ({ nodes } = await api('GET', `/api/graph${app ? `?app=${app}` : ''}`)); } catch (e) { out.replaceChildren(h('div', { class: 'err' }, e.message)); return; }
  if (!nodes.length) { out.replaceChildren(h('div', { class: 'hint' }, 'Nothing yet: build an app first.')); return; }
  const side = h('div', { class: 'gside' }, h('div', { class: 'hint' }, 'Click a node to see what it needs, what depends on it and what goes with it.'));
  let picked = null;
  const pick = async (id) => {
    if (!id || id === picked) return;
    picked = id;
    side.replaceChildren(h('div', { class: 'hint' }, 'Asking ChloeDB...'));
    const panel = await nodePanel(id, { h, api, showCode, onPick: (x) => g.select(x) });
    if (picked === id) side.replaceChildren(panel);
  };
  const g = drawGraph(nodes, { onSelect: pick, compact: !!app, selected });
  const edges = nodes.reduce((n, x) => n + (x.links || []).length, 0);
  out.replaceChildren(
    h('div', { class: 'where' }, GRAPH_HELP, ` ${nodes.length} nodes, ${edges} edges.`),
    h('div', { class: 'glegend' }, g.legend.map((l) => h('span', null, h('i', { style: `border-top:2px ${l.dash ? 'dashed' : 'solid'} ${l.color}` }), l.text))),
    h('div', { class: 'gwrap' }, h('div', { class: 'gcanvas' }, g.svg), side));
}

async function showMap(appId, name) {
  const modal = $('#modal');
  const close = () => { modal.hidden = true; modal.replaceChildren(); };
  modal.onclick = (e) => { if (e.target === modal) close(); };
  const body = h('div', { class: 'mapbody' }, h('div', { class: 'hint' }, 'Loading...'));
  modal.replaceChildren(h('div', { class: 'sheet wide' }, h('header', h('span', { class: 't' }, `${name}: how its pieces link`), h('button', { class: 'btn small ghost', onclick: close }, 'Close')), body));
  modal.hidden = false;
  await libraryGraph(body, { app: appId });
}

// ---- a build's timeline: what took the time, and what was decided for each piece

const KIND_LABEL = { strata: 'ChloeDB', plan: 'Model planning', model: 'Model planning and writing in one answer', write: 'Model writing pieces', fix: 'Model fixing', check: 'Checks' };
const DECISION = {
  use: ['Used', 'as it is'], copy: ['Copied', 'as it is'], keep: ['Kept', 'as it was'], new: ['New', 'written from scratch'],
  change: ['Rewrote', 'changed'], fork: ['Forked', 'started from another'], version: ['New version', 'of a shared piece'],
  upgrade: ['Newer', 'moved to a newer version'], remove: ['Removed', ''],
};
const secs = (ms) => ms < 1000 ? `${(ms / 1000).toFixed(1)} s` : ms < 100000 ? `${(ms / 1000).toFixed(ms < 10000 ? 1 : 0)} s` : `${Math.floor(ms / 60000)} min ${Math.round((ms % 60000) / 1000)} s`;
const money = (c) => `$${(c || 0).toFixed(2)}`;
const kTok = (n) => n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k` : String(n || 0);
const pieceName = (d) => `${d.kind === 'component' ? 'ui.' : d.kind === 'helper' ? 'lib.' : d.kind === 'table' ? 'table ' : `${d.kind} `}${d.name}`;

function whereFrom(d) {
  const f = d.from || {};
  if (d.decision === 'use') return f.builtin ? 'built-in, used as it is' : 'from the library (another app wrote it), used as it is';
  if (d.decision === 'copy') return f.starter ? 'built-in starter, copied as it is' : `copied from another app: ${f.title}`;
  if (d.decision === 'fork') return `forked from ${f.starter ? 'the built-in starter ' : ''}${f.title}`;
  if (d.decision === 'version') return `new version of the shared ${d.kind} (${f.author === 'builtin' ? 'built-in' : 'from another app'})`;
  if (d.decision === 'change') return d.kind === 'table' ? d.detail : d.patched ? `edited in place: ${d.patched} change${d.patched === 1 ? '' : 's'} to the screen, not rewritten` : d.patterns ? `rewritten, as a screen from the pattern${d.patterns.length > 1 ? 's' : ''} ${d.patterns.join(', ')}${d.statement ? ' (a CREATE SCREEN statement)' : ''}` : d.screen ? 'rewritten, as a screen (data, no code)' : 'rewritten';
  if (d.decision === 'new') return d.kind === 'table' ? d.detail : d.patterns ? `written as a screen from the pattern${d.patterns.length > 1 ? 's' : ''} ${d.patterns.join(', ')}${d.statement ? ' (a CREATE SCREEN statement)' : ''}` : d.screen ? 'written from scratch as a screen (data, no code)' : 'written from scratch';
  if (d.decision === 'upgrade') return 'moved to a newer version';
  return DECISION[d.decision]?.[1] || '';
}

// Where a build's time went, by kind of work: the wall time each kind held
// (pieces written at once count once), and the model time in all.
function timeSplit(trace) {
  const by = {};
  const model = { ms: 0, calls: 0, cost: 0, in: 0, out: 0, cached: 0 };
  for (const s of trace.spans) {
    const k = s.kind;
    (by[k] ||= []).push([s.start, s.end]);
    if (s.tokens) { model.ms += s.end - s.start; model.calls++; model.cost += s.cost || 0; model.in += (s.tokens.in || 0) + (s.tokens.cacheWrite || 0) + (s.tokens.cached || 0); model.cached += s.tokens.cached || 0; model.out += s.tokens.out || 0; }
  }
  const wall = {};
  for (const [k, list] of Object.entries(by)) {
    list.sort((a, b) => a[0] - b[0]);
    let total = 0, cs = -1, ce = -1;
    for (const [a, b] of list) { if (a > ce) { if (ce > cs) total += ce - cs; cs = a; ce = b; } else ce = Math.max(ce, b); }
    if (ce > cs) total += ce - cs;
    wall[k] = total;
  }
  return { wall, model };
}

function ganttRows(trace) {
  const total = Math.max(trace.ms, 1);
  const pct = (t) => `${Math.min(100, (t / total) * 100).toFixed(2)}%`;
  const step = [1, 2, 5, 10, 15, 20, 30, 60, 120, 300].find((s) => total / 1000 / s <= 6) || 600;
  const ticks = [];
  for (let t = 0; t <= total / 1000; t += step) ticks.push(t);
  const grid = () => ticks.slice(1).map((t) => h('span', { class: 'grid', style: `left:${pct(t * 1000)}` }));
  const decided = new Map(trace.decisions.filter((d) => d.piece).map((d) => [d.piece, d]));
  const rows = [h('div'), h('div', { class: 'axis' }, ticks.map((t) => h('span', { style: `left:${pct(t * 1000)}` }, `${t} s`)))];
  const at = trace.decisions.find((d) => d.t)?.t;
  for (const s of trace.spans) {
    const d = s.piece ? decided.get(s.piece) : null;
    const len = s.end - s.start;
    const tip = [s.label, `${secs(s.start)} to ${secs(s.end)} (${secs(len)})`,
      s.cost ? `${money(s.cost)}, ${kTok((s.tokens?.in || 0) + (s.tokens?.cacheWrite || 0) + (s.tokens?.cached || 0))} tokens in (${kTok(s.tokens?.cached)} cached), ${kTok(s.tokens?.out)} out` : null,
      s.chars ? `${s.chars.toLocaleString('en-US')} characters answered` : null,
      s.problems?.length ? `problems: ${s.problems.join('; ')}` : null].filter(Boolean).join('\n');
    // The bar's numbers go inside a long bar, else beside it, on the side with room.
    const place = len / total > 0.3 ? ' in' : s.end / total > 0.75 ? ' right' : '';
    rows.push(h('div', { class: 'lab', title: s.label }, d ? h('span', { class: `badge d-${d.decision}` }, DECISION[d.decision][0]) : null, h('span', { class: 'nm' }, s.label)),
      h('div', { class: 'track' }, grid(), h('span', { class: `bar k-${s.kind}${place}`, title: tip, style: `left:${pct(s.start)};width:${pct(len)}` },
        h('em', `${secs(len)}${s.cost ? ` · ${money(s.cost)}` : ''}${s.problems?.length ? ` · ${s.problems.length} problem${s.problems.length === 1 ? '' : 's'}` : ''}`))));
    if (at && (s.kind === 'plan' || s.kind === 'model') && Math.abs(s.end - at) < 2) {
      const n = trace.decisions.length;
      rows.push(h('div', { class: 'lab' }, h('b', 'Decided')), h('div', { class: 'track', style: 'background:none' }, grid(),
        h('span', { class: 'mark', style: `left:${pct(at)}`, title: `${n} decisions, made when the plan was ready` }),
        h('span', { class: `bar${at / total > 0.45 ? ' right' : ''}`, style: `left:${pct(at)};width:0;background:none` }, h('em', `${n} decisions at ${secs(at)}`))));
    }
  }
  return h('div', { class: 'gantt' }, rows);
}

function decisionList(trace) {
  const order = ['new', 'change', 'fork', 'version', 'copy', 'use', 'upgrade', 'remove', 'keep'];
  const writers = new Map();
  for (const s of trace.spans) if (s.piece) { const w = writers.get(s.piece) || { ms: 0, cost: 0 }; w.ms += s.end - s.start; w.cost += s.cost || 0; writers.set(s.piece, w); }
  const list = trace.decisions.slice().sort((a, b) => order.indexOf(a.decision) - order.indexOf(b.decision));
  const kept = list.filter((d) => d.decision === 'keep');
  const shown = list.filter((d) => d.decision !== 'keep');
  const items = shown.flatMap((d) => {
    const w = d.piece ? writers.get(d.piece) : null;
    const cost = w ? `${secs(w.ms)} · ${money(w.cost)}` : d.chars ? `${d.chars.toLocaleString('en-US')} chars, in the one answer` : ['use', 'copy', 'upgrade'].includes(d.decision) ? 'no model time' : '';
    return [h('span', { class: `badge d-${d.decision}` }, DECISION[d.decision]?.[0] || d.decision),
      h('div', { class: 'what' }, h('span', { class: 'nm mono' }, pieceName(d)), d.title && d.title !== d.name ? ` ${d.title}` : '', h('span', { class: 'muted' }, ` · ${whereFrom(d)}`),
        d.detail && !['table'].includes(d.kind) ? h('div', { class: 'why' }, String(d.detail)) : null),
      h('span', { class: 'cost' }, cost)];
  });
  if (kept.length) items.push(h('span', { class: 'badge d-keep' }, 'Kept'), h('div', { class: 'what muted' }, kept.map(pieceName).join(', ')), h('span', { class: 'cost' }, 'no model time'));
  return h('div', { class: 'dec' }, items);
}

function decisionCounts(decisions) {
  const n = {};
  for (const d of decisions) if (d.kind !== 'table') n[d.decision] = (n[d.decision] || 0) + 1;
  return h('div', { class: 'tl-sum' }, Object.entries(DECISION).filter(([k]) => n[k]).map(([k, [label]]) => h('span', { class: `badge d-${k}` }, `${label} ${n[k]}`)));
}

function timelineBody(trace) {
  const { wall, model } = timeSplit(trace);
  const kinds = ['strata', 'plan', 'model', 'write', 'check', 'fix'].filter((k) => wall[k]);
  const sumWall = kinds.reduce((a, k) => a + wall[k], 0) || 1;
  const writes = trace.spans.filter((s) => s.kind === 'write' || s.kind === 'fix');
  const facts = [
    h('span', h('b', secs(trace.ms)), ' in all'),
    h('span', h('b', money(trace.cost ?? model.cost)), ` for ${model.calls} model call${model.calls === 1 ? '' : 's'}`),
    h('span', h('b', `${kTok(model.in)}`), ` tokens in (${kTok(model.cached)} cached), `, h('b', kTok(model.out)), ' out'),
    writes.length > 1 ? h('span', h('b', secs(writes.reduce((a, s) => a + s.end - s.start, 0))), ` of model time writing ${writes.length} pieces at once in ${secs((wall.write || 0) + (wall.fix || 0))}`) : null,
    trace.status === 'failed' ? h('span', { class: 'err' }, `Failed: ${trace.error}`) : null,
  ];
  return [
    h('div', h('div', { class: 'tl-req' }, `“${trace.text}”`), h('div', { class: 'tl-facts', style: 'margin-top:6px' }, facts)),
    h('div', h('h3', 'Where the time went'),
      h('div', { class: 'tl-split' }, kinds.map((k) => h('span', { class: `k-${k}`, style: `flex:${wall[k]}`, title: `${KIND_LABEL[k]}: ${secs(wall[k])}` }))),
      h('div', { class: 'tl-key' }, kinds.map((k) => h('span', h('i', { class: `k-${k}` }), `${KIND_LABEL[k]} ${secs(wall[k])} (${Math.round((wall[k] / sumWall) * 100)}%)`)))),
    h('div', h('h3', 'Timeline'), ganttRows(trace)),
    trace.decisions.length ? h('div', h('h3', 'What was decided for each piece'), decisionCounts(trace.decisions), h('div', { style: 'height:10px' }), decisionList(trace)) : null,
    trace.log?.length ? h('details', h('summary', { class: 'small muted', style: 'cursor:pointer' }, `What the build said (${trace.log.length} lines)`),
      h('div', { class: 'tl-log', style: 'margin-top:8px' }, trace.log.map((l) => [h('span', { class: 't' }, secs(l.t)), h('span', l.line)]))) : null,
  ].filter(Boolean);
}

async function showTimeline(requestId, heading) {
  const modal = $('#modal');
  const close = () => { modal.hidden = true; modal.replaceChildren(); };
  modal.onclick = (e) => { if (e.target === modal) close(); };
  const body = h('div', { class: 'tl' }, h('div', { class: 'hint' }, 'Loading...'));
  modal.replaceChildren(h('div', { class: 'sheet wide' }, h('header', h('span', { class: 't' }, heading), h('button', { class: 'btn small ghost', onclick: close }, 'Close')), body));
  modal.hidden = false;
  try {
    const { trace } = await api('GET', `/api/apps/${S.appId}/traces/${encodeURIComponent(requestId)}`);
    body.replaceChildren(...timelineBody(trace));
  } catch (e) { body.replaceChildren(h('div', { class: 'hint' }, e.message)); }
}

// Every build of the app with a timeline, summed up.
async function buildStats(box) {
  let traces;
  try { ({ traces } = await api('GET', `/api/apps/${S.appId}/traces`)); } catch { return; }
  if (!traces.length) { box.remove(); return; }
  let ms = 0, cost = 0, model = 0;
  const wall = {};
  for (const t of traces) {
    ms += t.ms; cost += t.cost || 0;
    const sp = timeSplit(t);
    model += sp.model.ms;
    for (const [k, v] of Object.entries(sp.wall)) wall[k] = (wall[k] || 0) + v;
  }
  const kinds = ['strata', 'plan', 'model', 'write', 'check', 'fix'].filter((k) => wall[k]);
  const sum = kinds.reduce((a, k) => a + wall[k], 0) || 1;
  box.replaceChildren(h('h2', { class: 'label' }, `Building it: ${traces.length} build${traces.length === 1 ? '' : 's'}`),
    h('div', { class: 'tl-facts' }, h('span', h('b', secs(ms)), ' in all'), h('span', h('b', money(cost))), h('span', h('b', secs(model)), ' of model time')),
    h('div', { class: 'tl-split', style: 'height:14px' }, kinds.map((k) => h('span', { class: `k-${k}`, style: `flex:${wall[k]}`, title: `${KIND_LABEL[k]}: ${secs(wall[k])}` }))),
    h('div', { class: 'tl-key' }, kinds.map((k) => h('span', h('i', { class: `k-${k}` }), `${KIND_LABEL[k]} ${Math.round((wall[k] / sum) * 100)}%`))),
    decisionCounts(traces.flatMap((t) => t.decisions || [])));
}

// ---- code

// SQL with its keywords, strings and comments told apart (bodies in
// $$ ... $$, JavaScript or JSON, are shown as they are).
const SQL_TOKENS = /(--[^\n]*)|('(?:[^']|'')*')|(\$[A-Za-z0-9_]*\$[\s\S]*?\$[A-Za-z0-9_]*\$)|\b([A-Z][A-Z_]{1,}(?:\s+BY)?)\b/g;
function sqlNodes(text) {
  const out = [];
  let at = 0;
  for (const m of String(text).matchAll(SQL_TOKENS)) {
    if (m.index > at) out.push(text.slice(at, m.index));
    out.push(m[1] ? h('span', { class: 'c' }, m[1]) : m[2] ? h('span', { class: 's' }, m[2]) : m[3] ? m[3] : h('span', { class: 'k' }, m[4]));
    at = m.index + m[0].length;
  }
  out.push(text.slice(at));
  return out;
}

async function showCode(id, newer) {
  const modal = $('#modal');
  const close = () => { modal.hidden = true; modal.replaceChildren(); };
  modal.onclick = (e) => { if (e.target === modal) close(); };
  let cap;
  try { cap = await api('GET', `/api/capabilities/${encodeURIComponent(id)}`); } catch (e) { toast(e.message); return; }
  const v = Number(cap.version);
  const prev = v > 1 ? id.replace(/@\d+$/, `@${v - 1}`) : null;
  // Each piece is stored as its SQL; a screen or a rule also shows what it
  // compiles into (the JSON the engine runs).
  const sql = cap.sql || cap.spec?.statement || cap.code;
  const compiled = (cap.kind === 'view' && cap.spec?.screen) || (cap.kind === 'function' && cap.spec?.rule) ? cap.code : null;
  const pre = h('pre', { class: 'sql' }, sqlNodes(sql));
  const shown = { now: 'sql' };
  const seg = compiled ? h('div', { class: 'seg' },
    ...[['sql', 'SQL'], ['compiled', 'Compiled']].map(([k, label]) => h('button', { class: k === 'sql' ? 'on' : '', onclick: (e) => {
      shown.now = k;
      pre.replaceChildren(...(k === 'sql' ? sqlNodes(sql) : [compiled]));
      for (const b of e.target.parentNode.children) b.classList.toggle('on', b === e.target);
    } }, label))) : null;
  const copy = h('button', { class: 'btn small', onclick: async () => { try { await navigator.clipboard.writeText(pre.textContent); toast('Copied'); } catch { toast('Copy did not work here: select the text instead'); } } }, 'Copy');
  const trigger = cap.spec?.on ? `runs after each ${[].concat(cap.spec.on.event || 'insert').join(' or ')} on ${cap.spec.on.table}` : null;
  modal.replaceChildren(h('div', { class: 'sheet' },
    h('header', h('span', { class: `pill ${cap.kind}` }, cap.kind), h('span', { class: 't' }, `${cap.title} · v${v}`),
      prev ? h('button', { class: 'btn small', title: 'the version before', onclick: () => showCode(prev, id) }, `← v${v - 1}`) : null,
      newer ? h('button', { class: 'btn small', onclick: () => showCode(newer) }, `v${v + 1} →`) : null,
      seg, copy,
      h('button', { class: 'btn small ghost', onclick: close }, 'Close')),
    h('div', { class: 'info' },
      cap.description ? h('div', cap.description) : null,
      trigger ? h('div', trigger) : null,
      h('div', h('span', { class: 'mono' }, cap.id), ` · written by ${cap.author || 'forge'} ${ago(cap.created)}`, cap.forked_from ? ` · from ${cap.forked_from}` : ''),
      h('div', `A row of sa_capabilities in ChloeDB, stored as this SQL${compiled ? ' (compiled when it is read)' : ''}${cap.kind === 'table' ? `; its data is the table app_${cap.app}_${cap.slug}` : ''}.`)),
    pre));
  modal.hidden = false;
}

// The whole app as one SQL script: every table, screen, rule and function,
// and the shared pieces it uses.
async function showAppSql(appId) {
  const modal = $('#modal');
  const close = () => { modal.hidden = true; modal.replaceChildren(); };
  modal.onclick = (e) => { if (e.target === modal) close(); };
  let out;
  try { out = await api('GET', `/api/apps/${appId}/source`); } catch (e) { toast(e.message); return; }
  const file = `${String(out.name || appId).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '') || appId}.sql`;
  const download = () => {
    const url = URL.createObjectURL(new Blob([out.sql], { type: 'application/sql' }));
    const a = h('a', { href: url, download: file });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };
  const lines = out.sql.split('\n').length;
  modal.replaceChildren(h('div', { class: 'sheet wide' },
    h('header', h('span', { class: 'pill view' }, 'sql'), h('span', { class: 't' }, `${out.name} as SQL`),
      h('button', { class: 'btn small', onclick: async () => { try { await navigator.clipboard.writeText(out.sql); toast('Copied'); } catch { toast('Copy did not work here: select the text instead'); } } }, 'Copy'),
      h('button', { class: 'btn small', onclick: download }, `Download ${file}`),
      h('button', { class: 'btn small ghost', onclick: close }, 'Close')),
    h('div', { class: 'info' }, h('div', `Every piece of the app as the SQL it is stored as: ${lines} lines. Tables are CREATE TABLE, screens CREATE SCREEN, rules CREATE RULE, and pieces written in JavaScript a CREATE ... LANGUAGE javascript.`)),
    h('pre', { class: 'sql' }, sqlNodes(out.sql))));
  modal.hidden = false;
}

// ---- an app

async function openApp(id, viewSlug, activity = false) {
  const first = S.appId !== id;
  if (first) {
    leaveApp();
    S.appId = id;
    S.panel = 'change';
    await assets();
    $('#main').replaceChildren(h('div', { class: 'appview' },
      h('section', { class: 'stage' }, h('div', { class: 'tabs', id: 'tabs' }), h('div', { class: 'frames', id: 'frames' })),
      h('aside', { class: 'panel closed', id: 'panel' })));
  }
  if (viewSlug) S.view = viewSlug;
  if (!BARE) S.activity = activity;
  await refreshApp();
}

let refreshing = null;
let again = false;
async function refreshApp() {
  if (!S.appId) return;
  if (refreshing) { again = true; return refreshing; }
  refreshing = (async () => {
    try {
      const state = await api('GET', `/api/apps/${S.appId}${atV()}`);
      if (state.app.id !== S.appId) return;
      S.app = state;
      // A read that started before the build ended still lists it as running.
      const run = state.running && !S.ended.has(state.running.id) ? state.running : null;
      if (run && !S.lines.length) { S.runningText = run.text; S.lines = run.lines; S.runningId = run.id; }
      if (run?.steps && S.runningText === run.text) S.build = { first: run.first, split: run.split, steps: run.steps, pieces: run.pieces || [], elapsed: run.elapsed || 0, expect: run.expect || {}, at: Date.now() };
      if (!run && S.lines.length && (!S.runningText || S.ended.has(S.runningId))) { S.lines = []; S.build = null; S.runningText = ''; }
      renderApp();
    } catch (e) {
      $('#main').replaceChildren(h('div', { class: 'page' }, h('div', { class: 'err' }, e.message), h('a', { href: '#/' }, 'Back')));
      S.appId = null;
    }
  })();
  await refreshing;
  refreshing = null;
  if (again) { again = false; await refreshApp(); }
}

function renderApp() {
  const st = S.app;
  applyDesign();
  if (BARE) {
    document.title = st.app.name;
    $('.brand').replaceChildren(h('span', { class: 'mark' }), h('span', { class: 'name' }, st.app.name));
    renderLook();
  } else {
    $('#crumbs').replaceChildren(...[h('span', { class: 'sep' }, '/'), h('b', st.app.name), st.version.n ? h('span', { class: 'pill' }, st.version.trying ? `trying v${st.version.trying} (live v${st.version.live})` : `v${st.version.n}`) : null,
      st.views.length ? h('button', { class: `navlink small act-open${S.activity ? ' on' : ''}`, title: 'What people did in this app', 'aria-pressed': String(!!S.activity), onclick: () => (S.activity ? switchView(S.view) : showActivity()) }, h('span', { class: 'pulse' }), 'Activity') : null,
      st.views.length ? h('a', { class: 'navlink small own', href: `/a/${st.app.id}`, target: '_blank', title: 'The app without the studio: for a phone, or to hand to someone' }, 'Open on its own') : null].filter(Boolean));
  }
  const views = st.views;
  if (!views.find((v) => v.slug === S.view)) S.view = views[0]?.slug || null;
  // The first screen shown is opened too.
  if (S.view && !S.viewSince && !S.activity) {
    S.viewSince = Date.now();
    const first = views.find((v) => v.slug === S.view);
    if (first) api('POST', `/api/apps/${S.appId}/track${atV()}`, { kind: 'open', detail: S.view, cap: first.id }).catch(() => {});
  }
  // tabs
  const tabs = $('#tabs');
  tabs.replaceChildren(...views.map((v) => h('button', { class: `tab${v.slug === S.view && !S.activity ? ' on' : ''}`, 'data-slug': v.slug, onclick: () => { S.taps = (S.taps || 0) + 1; switchView(v.slug); } }, tabIcon(v.title), h('span', { class: 'tlabel' }, v.title), S.errors.has(v.slug) ? h('span', { class: 'dot', style: 'background:var(--bad)' }) : null)),
    h('button', { class: 'tab mobile-only', style: 'margin-left:auto', onclick: () => togglePanel() }, st.running ? 'Changing...' : 'Change'));
  if (!views.length) tabs.replaceChildren(h('span', { class: 'tab on' }, 'First version'), h('button', { class: 'tab mobile-only', style: 'margin-left:auto', onclick: () => togglePanel() }, 'Details'));
  renderTryBar();
  renderFrames();
  renderPanel();
}

// An icon for a screen, from the words of its title (the bold look shows
// them in a tab bar at the bottom of a phone).
const ICONS = [
  [/^(add|new)\b/i, 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM12 8v8M8 12h8'],
  [/^edit\b|editor/i, 'M4 20h4L19 9l-4-4L4 16v4ZM13.5 6.5l4 4'],
  [/cart|basket|checkout/i, 'M3 4h2l2.5 11h11L21 7H6.5M9 20a1 1 0 1 0 0 .01M18 20a1 1 0 1 0 0 .01'],
  [/stock|inventor|item|product|pantry|supplies|shop|store/i, 'M21 8 12 3 3 8v8l9 5 9-5V8ZM3 8l9 5 9-5M12 13v8'],
  [/order|deliver|ship|purchase|supplier|vendor/i, 'M3 6h11v10H3zM14 9h4l3 3v4h-7M7 18a2 2 0 1 0 0 .01M17 18a2 2 0 1 0 0 .01'],
  [/rent|pay|money|budget|expense|bill|invoice|sale|income|cost|price|fund/i, 'M3 7h18v12H3zM3 11h18M7 15h3'],
  [/tenant|people|member|client|customer|guest|contact|team|staff|student|patient|famil|kid|friend|roster|player/i, 'M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8ZM4 21a8 8 0 0 1 16 0'],
  [/calendar|schedule|week|month|day|event|booking|appointment|shift|plan/i, 'M3 5h18v16H3zM3 10h18M8 3v4M16 3v4'],
  [/repair|fix|maint|issue|ticket|bug/i, 'M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18l3 3 6.3-6.3a4 4 0 0 0 5.4-5.4l-2.6 2.6-2.4-.6-.6-2.4Z'],
  [/request|idea|feature|feedback|suggest/i, 'M9 18h6M10 21h4M12 3a6 6 0 0 0-4 10.5c.8.8 1 1.5 1 2.5h6c0-1 .2-1.7 1-2.5A6 6 0 0 0 12 3Z'],
  [/report|stat|chart|insight|summary|dashboard|trend|analytics|vote/i, 'M4 20V10M10 20V4M16 20v-7M22 20H2'],
  [/history|log|activity|record|journal|diary/i, 'M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18ZM12 7v5l3 2'],
  [/setting|admin|config|option/i, 'M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6ZM19 12a7 7 0 0 0-.1-1.2l2-1.6-2-3.4-2.4 1a7 7 0 0 0-2-1.2L14 3h-4l-.5 2.6a7 7 0 0 0-2 1.2l-2.4-1-2 3.4 2 1.6a7 7 0 0 0 0 2.4l-2 1.6 2 3.4 2.4-1a7 7 0 0 0 2 1.2L10 21h4l.5-2.6a7 7 0 0 0 2-1.2l2.4 1 2-3.4-2-1.6c.1-.4.1-.8.1-1.2Z'],
  [/home|overview|today|dashboard|start/i, 'm3 11 9-8 9 8v9a1 1 0 0 1-1 1h-5v-6H9v6H4a1 1 0 0 1-1-1z'],
  [/task|todo|to do|chore|check|habit|list/i, 'M9 11l3 3 8-8M20 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11'],
  [/note|book|read|paper|article|librar|study|learn|lesson/i, 'M5 4h11a3 3 0 0 1 3 3v13H8a3 3 0 0 1-3-3V4ZM5 17a3 3 0 0 1 3-3h11M9 8h6'],
  [/link|source|bookmark/i, 'M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1'],
  [/quiz|question|test|exam|ask/i, 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM9.5 9a2.5 2.5 0 1 1 3.5 2.3c-.6.3-1 .9-1 1.6V14M12 17v.01'],
  [/card|deck|flash/i, 'M8 4h11v14H8zM5 7v13h11'],
  [/menu|dish|food|recipe|meal|kitchen/i, 'M7 3v18M4 3v5a3 3 0 0 0 6 0V3M17 21V3c-2 1-3 3-3 6v5h3'],
  [/flat|unit|room|house|propert|building|apartment/i, 'M4 21V5l8-3v19M12 9h8v12M3 21h18M8 7v.01M8 11v.01M8 15v.01M16 13v.01M16 17v.01'],
  [/adjust|count|move|transfer/i, 'M4 7h10M18 7h2M4 17h2M10 17h10M16 4v6M8 14v6'],
  [/run|workout|training|exercise|activity|fitness/i, 'M3 12h4l3-8 4 16 3-8h4'],
  [/race|goal|milestone|target/i, 'M5 21V4M5 4h11l-2 4 2 4H5'],
  [/zone|area|region|map|route|road|place|location/i, 'M9 4 3 6v14l6-2 6 2 6-2V4l-6 2-6-2ZM9 4v14M15 6v14'],
  [/loan|debt|allowance|wallet|point|reward/i, 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM15 9.5c-.5-1-1.6-1.5-3-1.5-1.7 0-3 .8-3 2s1.3 1.7 3 2 3 .8 3 2-1.3 2-3 2c-1.4 0-2.5-.5-3-1.5M12 6v2M12 16v2'],
];
function tabIcon(title) {
  const d = (ICONS.find(([re]) => re.test(title)) || [null, 'M4 6h16M4 12h16M4 18h10'])[1];
  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', 'ticon');
  svg.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS(NS, 'path');
  path.setAttribute('d', d);
  svg.append(path);
  return svg;
}

// ---- trying a version the engine built from use

function tryVersion(n) {
  S.tryV = n || null;
  if (BARE) {
    const u = new URL(location.href);
    if (S.tryV) u.searchParams.set('v', String(S.tryV)); else u.searchParams.delete('v');
    history.replaceState(null, '', u);
  }
  refreshApp();
}
async function decide(c, action) {
  try {
    const out = await api('POST', `/api/apps/${S.appId}/candidates/${c.id}`, { action });
    if (S.tryV === c.n) S.tryV = null;
    toast(action === 'drop' ? 'Dropped: the app stays as it is' : out.how === 'rebuilt' ? 'The live version changed the same pieces: building it again on the live version' : `Kept: version ${out.head} is live (${out.how})`);
  } catch (e) { toast(e.message); }
  refreshApp();
}
function renderTryBar() {
  const stage = $('.stage');
  if (!stage) return;
  $('#trybar')?.remove();
  const st = S.app;
  if (!st.version.trying) return;
  const c = (st.evolve?.candidates || []).find((x) => x.n === st.version.trying);
  const bar = h('div', { id: 'trybar', class: 'trybar' },
    h('span', { class: 'tryicon' }, '✦'),
    h('div', { class: 'grow' },
      h('b', c ? c.text : `Version ${st.version.trying}`),
      h('div', { class: 'hint', title: c?.why || '' }, c ? `Built from your use. You are trying version ${st.version.trying} on the app's real data; everyone else sees version ${st.version.live}.` : `An earlier or unpublished version; the live one is ${st.version.live}.`)),
    c && c.status === 'ready' && !BARE ? h('button', { class: 'btn small primary', onclick: () => decide(c, 'keep') }, 'Keep') : null,
    c && c.status === 'ready' && !BARE ? h('button', { class: 'btn small', onclick: () => decide(c, 'drop') }, 'Drop') : null,
    h('button', { class: 'btn small ghost', onclick: () => tryVersion(null) }, 'Back to live'));
  stage.prepend(bar);
}

// ---- designs: a block of CSS variables, posted to every frame and set on the
// stage around them; nothing is rebuilt.

const lookKey = () => `sa-design-${S.appId}`;
function ownLook() {
  if (!BARE) return null;
  try { const v = localStorage.getItem(lookKey()); return v === null ? null : Number(v); } catch { return null; }
}
// ?look=bold (or plain) shows any app in that look, to compare looks on the
// same app; the app's own design is unchanged.
const LOOK_PARAM = new URLSearchParams(location.search).get('look');
function currentDesign() {
  const d = S.app?.design;
  const chosen = !d ? checkDesign(PRESETS[0]) : (() => { const own = ownLook(); return d.designs[own !== null && d.designs[own] ? own : d.chosen] || d.designs[0]; })();
  // a design stored before looks existed has none: checking it gives the default
  return checkDesign(LOOK_PARAM ? { ...chosen, look: LOOK_PARAM } : chosen, chosen);
}
function applyDesign() {
  const d = currentDesign();
  const css = designCss(d);
  const nav = `nav-${d.nav}`;
  const look = `look-${d.look}`;
  const stage = $('.stage');
  for (const el of [stage, BARE ? document.body : null].filter(Boolean)) {
    el.classList.add('designed');
    for (const c of [...el.classList]) if ((c.startsWith('nav-') && c !== nav) || (c.startsWith('look-') && c !== look)) el.classList.remove(c);
    el.classList.add(nav, look);
  }
  if (css === S.designCss) return;
  S.designCss = css;
  let style = document.getElementById('shell-design');
  if (!style) { style = h('style', { id: 'shell-design' }); document.head.append(style); }
  style.textContent = designCss(d, BARE ? 'body.bare' : '.stage');
  for (const f of S.frames.values()) f.iframe.contentWindow?.postMessage({ __sa: 1, kind: 'design', css }, '*');
  for (const f of $('#frames')?.querySelectorAll('.preview iframe') || []) f.contentWindow?.postMessage({ __sa: 1, kind: 'design', css }, '*');
  if (!S.app?.design && S.appId && !S.askedDesigns) {
    // An app made before designs gets its three on first open.
    S.askedDesigns = S.appId;
    api('GET', `/api/apps/${S.appId}/designs`).then(() => refreshApp()).catch(() => {});
  }
}
async function chooseDesign(i) {
  const d = S.app.design;
  if (!d || !d.designs[i]) return;
  if (BARE) {
    try { localStorage.setItem(lookKey(), String(i)); } catch { /* the choice lasts this visit */ }
    applyDesign();
    renderLook();
    return;
  }
  d.chosen = i;
  applyDesign();
  markChosen();
  try { await api('POST', `/api/apps/${S.appId}/designs`, { chosen: i }); } catch (e) { toast(e.message); }
}

// The three looks, in a few words.
const LOOK_WORDS = { native: 'phone lists', bold: 'bold cards', plain: 'tables and tiles' };
// The Design tab: the three designs, each a live miniature of the view open
// now with the app's own data, its navigation drawn around it.
function designTab() {
  const st = S.app;
  const d = st.design;
  if (!d) {
    return [h('div', { class: 'where' }, h('span', { class: 'spin' }), ' Designing three looks for this app (a few seconds)...')];
  }
  const v = st.views.find((x) => x.slug === S.view) || st.views[0];
  const cards = d.designs.map((x, i) => {
    const iframe = h('iframe', { sandbox: 'allow-scripts', title: `${x.name}: a preview`, tabindex: '-1' });
    if (v) iframe.srcdoc = srcdoc(v, S.params?.[v.slug] || {}, st.components, st.helpers, designCss(x));
    const tabs = st.views.slice(0, 5).map((w) => h('span', { class: `tab${w.slug === v?.slug ? ' on' : ''}` }, w.title));
    return h('button', { class: `dcard${i === d.chosen ? ' on' : ''}`, 'data-i': String(i), type: 'button', onclick: () => chooseDesign(i), title: `Use ${x.name}` },
      h('style', null, designCss(x, `.dcard[data-i="${i}"] .mini`)),
      h('div', { class: 'mwrap' }, h('div', { class: `mini designed nav-${x.nav} look-${x.look}` }, h('div', { class: 'tabs' }, tabs), h('div', { class: 'mframe' }, iframe))),
      h('div', { class: 'dmeta' },
        h('div', { class: 'dname' }, h('span', { class: 'swatch', style: `background:${x.light.accent}` }), h('span', { class: 'swatch', style: `background:${x.dark.bg}` }), h('b', x.name), h('span', { class: 'pill inuse' }, 'In use')),
        x.about ? h('div', { class: 'hint' }, x.about) : null,
        h('div', { class: 'dtokens' }, `${LOOK_WORDS[x.look] || x.look} · ${x.titles} titles · ${x.corners} corners · ${x.nav} · ${x.scheme === 'auto' ? 'light and dark' : x.scheme}`)));
  });
  const again = h('button', { class: 'btn small', onclick: async () => {
    again.disabled = true;
    again.textContent = 'Designing...';
    try { await api('POST', `/api/apps/${S.appId}/designs/new`); S.designKey = ''; await refreshApp(); } catch (e) { toast(e.message); }
    again.disabled = false;
    again.textContent = 'New alternatives';
  } }, 'New alternatives');
  return [
    h('div', { class: 'where' }, 'Three designs for this app, three shapes of the same screens. Click one to use it: every view changes at once, with no rebuild. A design is a few tokens (its look, colors, type, corners, spacing, navigation). People using the app on its own can pick their own under Aa.'),
    h('div', { class: 'dcards' }, cards),
    h('div', { class: 'row', style: 'display:flex;gap:8px;align-items:center' }, again, h('span', { class: 'hint' }, 'keeps the one in use, replaces the other two')),
  ];
}
function markChosen() {
  const d = S.app?.design;
  for (const c of document.querySelectorAll('.dcard')) c.classList.toggle('on', Number(c.dataset.i) === d?.chosen);
}

// On the app's own page: Aa opens the three designs; the pick is this
// browser's own.
function renderLook() {
  if (!BARE) return;
  const right = $('.top-right');
  if (!right) return;
  let btn = $('#look');
  if (!btn) {
    btn = h('button', { id: 'look', class: 'look', type: 'button', title: 'Design', onclick: (e) => { e.stopPropagation(); $('#lookmenu')?.toggleAttribute('hidden'); } }, 'Aa');
    right.prepend(btn);
    document.addEventListener('click', (e) => { if (!e.target.closest?.('#lookmenu')) $('#lookmenu')?.setAttribute('hidden', ''); });
  }
  const d = S.app?.design;
  btn.hidden = !d;
  $('#lookmenu')?.remove();
  if (!d) return;
  const own = ownLook();
  const cur = own !== null && d.designs[own] ? own : d.chosen;
  const menu = h('div', { id: 'lookmenu', class: 'lookmenu', hidden: true },
    h('div', { class: 'lookhead' }, 'Design'),
    d.designs.map((x, i) => h('button', { class: `lookitem${i === cur ? ' on' : ''}`, type: 'button', onclick: () => chooseDesign(i) },
      h('span', { class: 'swatch', style: `background:${x.light.accent}` }), h('span', { class: 'swatch', style: `background:${x.dark.bg}` }),
      h('span', { class: 'grow' }, h('b', x.name), x.about ? h('div', { class: 'hint' }, x.about) : null), i === cur ? h('span', { class: 'check' }, '✓') : null)));
  right.append(menu);
}

function togglePanel() {
  S.panelOpen = !S.panelOpen;
  $('#panel').classList.toggle('closed', !S.panelOpen);
}

// Leaving a screen records how long it was looked at, and tells its frame,
// which records the forms typed into and left unsaved.
function leaveView(beacon) {
  const at = S.viewSince;
  const slug = S.view;
  S.viewSince = 0;
  if (!S.appId || !slug || !at) return;
  const prev = S.app?.views.find((x) => x.slug === slug);
  const body = { kind: 'leave', detail: slug, cap: prev?.id || '', ms: Date.now() - at, session: SESSION };
  if (beacon) { try { navigator.sendBeacon(`/api/apps/${S.appId}/track${atV()}`, new Blob([JSON.stringify(body)], { type: 'application/json' })); } catch {} }
  else {
    api('POST', `/api/apps/${S.appId}/track${atV()}`, body).catch(() => {});
    S.frames.get(slug)?.iframe.contentWindow?.postMessage({ __sa: 1, kind: 'leaving' }, '*');
  }
}
window.addEventListener('pagehide', () => leaveView(true));

// What people did in the app, over its screens, in the app's own design.
function showActivity() {
  leaveView(false);
  S.activity = true;
  history.replaceState(null, '', `#/app/${S.appId}/~activity`);
  renderApp();
}
function closeActivity() {
  S.act?.destroy();
  S.act = null;
  S.activity = false;
}

function switchView(slug, params) {
  if (slug !== S.view) leaveView(false);
  if (S.activity) { S.activity = false; S.viewSince = 0; }
  S.view = slug;
  S.viewSince = Date.now();
  const v = S.app.views.find((x) => x.slug === slug);
  if (v) api('POST', `/api/apps/${S.appId}/track${atV()}`, { kind: 'open', detail: slug, cap: v.id }).catch(() => {});
  if (params) {
    const f = S.frames.get(slug);
    if (f) { f.iframe.remove(); S.frames.delete(slug); }
    S.params = { ...(S.params || {}), [slug]: params };
  }
  history.replaceState(null, '', BARE ? `#/${encodeURIComponent(slug)}` : `#/app/${S.appId}/${encodeURIComponent(slug)}`);
  renderApp();
}

const noClose = (code) => String(code).replace(/<\/(script)/gi, '<\\/$1');

// A screen (a view written as data) is compiled here, before the frame loads:
// each "=expression" becomes a function in a script of its own (so one that
// does not parse breaks only itself), and each code block likewise.
function compileScreen(text) {
  let spec;
  try { spec = JSON.parse(text); } catch (e) { return { error: `the screen is not JSON: ${e.message}` }; }
  const exprs = [];
  const codes = [];
  const walk = (v, key) => {
    if (key === 'code' && typeof v === 'string') { codes.push(v); return { $c: codes.length - 1 }; }
    if (typeof v === 'string' && v.startsWith('=')) { exprs.push(v.slice(1)); return { $e: exprs.length - 1 }; }
    if (Array.isArray(v)) return v.map((x) => walk(x));
    if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, walk(x, k)]));
    return v;
  };
  const compiled = walk(spec);
  const scripts = [
    `<script>window.__SA_EXPR = []; window.__SA_CODE = []; window.__SA_EXPR_SRC = ${JSON.stringify(exprs).replace(/</g, '\\u003c')};</script>`,
    ...exprs.map((e, i) => `<script>__SA_EXPR[${i}] = function (s) { with (s) { return (${noClose(e)}\n); } };</script>`),
    ...codes.map((c, i) => `<script>__SA_CODE[${i}] = function () {\n${noClose(c)}\n;return typeof render === 'function' ? render : null; };</script>`),
  ];
  return { spec: compiled, scripts: scripts.join('\n') };
}

// The hosts the app's owner let it read from (app.fetch): its frames may
// show images from them too (web/policy.js).
const webHosts = () => (S.app?.reach?.on?.fetch ? (S.app.reach.on.hosts || []).map((x) => x.host) : []);

function srcdoc(view, params, components, helpers, design = S.designCss) {
  const screen = view.spec && view.spec.screen ? compileScreen(view.code) : null;
  const code = screen
    ? (screen.error ? `throw new Error(${JSON.stringify(screen.error)});` : `async function render(root, app) { await __sa_screen(root, app, ${JSON.stringify(screen.spec).replace(/</g, '\\u003c')}); }`)
    : noClose(view.code);
  // A visitor's access (a published app's page, or "See it as a stranger"):
  // the frame offers no write it would be refused. None for the owner.
  const access = S.app?.public ? S.app.access || 'look' : null;
  const boot = JSON.stringify({ slug: view.slug, cap: view.id, app: S.appId, origin: location.origin, params: params || {}, access }).replace(/</g, '\\u003c');
  // One script per shared component, so a broken one does not take the others down.
  const comps = (components || []).map((c) => `<script>__sa_component(${JSON.stringify(c.slug)}, ${JSON.stringify(c.id)}, function () {
${noClose(c.code)}
;return typeof component === 'function' ? component : null; });</script>`).join('\n');
  const libs = (helpers || []).map((c) => `<script>__sa_helper(${JSON.stringify(c.slug)}, ${JSON.stringify(c.id)}, function () {
${noClose(c.code)}
;return helper(); });</script>`).join('\n');
  return `<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="${frameCsp(location.origin, { hosts: webHosts() })}">
<style>${KIT}</style><style id="__design">${design || ''}</style></head><body><div id="root"></div>
<script>window.__SA = ${boot};</script>
<script>${FRAME}</script>
${screen ? `<script>${FRONT}</script>\n<script>${SCREEN}</script>\n${screen.scripts || ''}` : ''}
${libs}
${comps}
<script>
(async () => { try {
${code}
;if (typeof render !== 'function') throw new Error('the view defines no render(root, app)');
await render(document.getElementById('root'), app);
__sa_ready();
} catch (e) { __sa_report(e); } })();
</script></body></html>`;
}

// Screens the model has written while it writes the rest: drawn over the
// app with what data there is (none, for a first version).
function previewBox() {
  if (!S.previews.size || !S.lines.length) return null;
  const list = [...S.previews.values()].sort((a, b) => (a.order ?? 50) - (b.order ?? 50));
  const cur = S.previews.get(S.previewView) || list[0];
  const v = { slug: cur.slug, id: `preview:${cur.slug}`, title: cur.title, spec: { screen: true }, code: JSON.stringify({ ...cur.screen, preview: true }) };
  const iframe = h('iframe', { sandbox: 'allow-scripts', title: `Preview: ${cur.title}` });
  iframe.srcdoc = srcdoc(v, {}, S.app.components, S.app.helpers);
  return h('div', { class: 'preview' },
    h('div', { class: 'pbar' }, h('span', { class: 'spin' }), h('b', ' Preview'), h('span', { class: 'hint' }, ' while the model writes the rest; it goes live when the version is stored'),
      h('span', { class: 'bmini' }, h('span', { class: 'bbar' }, h('div', { class: 'fill', style: 'width:1%' })), h('span', { class: 'bmeta' }, paceText(S.build))),
      h('span', { class: 'ptabs-mini' }, list.map((p) => h('button', { class: `chip${p.slug === cur.slug ? ' on' : ''}`, onclick: () => { S.previewView = p.slug; renderFrames(); } }, p.title)))),
    iframe);
}

function renderFrames() {
  const box = $('#frames');
  const st = S.app;
  const key = S.previews.size && S.lines.length ? `${[...S.previews.keys()].join(',')}|${S.previewView}` : '';
  const shown = box.querySelector('.preview');
  if (key && shown?.dataset.key === key) return;
  shown?.remove();
  const pv = key ? previewBox() : null;
  if (pv) {
    pv.dataset.key = key;
    for (const f of S.frames.values()) f.iframe.hidden = true;
    if (S.act) S.act.box.hidden = true;
    box.querySelector('.building')?.remove();
    box.append(pv);
    return;
  }
  if (!st.views.length) {
    for (const f of S.frames.values()) f.iframe.remove();
    S.frames.clear();
    box.replaceChildren(buildingScreen());
    return;
  }
  box.querySelector('.building')?.remove();
  // drop frames of views that are gone or changed, or whose components changed
  const compSig = [...(st.components || []), ...(st.helpers || [])].map((c) => c.id).join(',') + `|${webHosts().join(' ')}`;
  for (const [slug, f] of S.frames) {
    const v = st.views.find((x) => x.slug === slug);
    if (!v || v.id !== f.capId || f.comps !== compSig) { f.iframe.remove(); S.frames.delete(slug); S.errors.delete(slug); }
  }
  const v = st.views.find((x) => x.slug === S.view);
  if (v && !S.frames.has(v.slug) && S.left?.get(v.slug) !== v.id) {
    const params = S.params?.[v.slug] || {};
    const iframe = h('iframe', { sandbox: FRAME_SANDBOX, title: v.title, allow: 'clipboard-write' });
    iframe.srcdoc = srcdoc(v, params, st.components, st.helpers);
    // A frame that navigates away from its page (location.href = ...) has
    // sent a request out; the view is closed rather than left there.
    let loads = 0;
    iframe.addEventListener('load', () => {
      if (++loads < 2 || S.frames.get(v.slug)?.iframe !== iframe) return;
      iframe.remove();
      S.frames.delete(v.slug);
      (S.left ||= new Map()).set(v.slug, v.id);
      S.errors.set(v.slug, { message: 'the view left its page (it navigated the frame), so it was closed' });
      renderApp();
    });
    box.append(iframe);
    S.frames.set(v.slug, { iframe, capId: v.id, comps: compSig, params });
  }
  for (const [slug, f] of S.frames) f.iframe.hidden = slug !== S.view || !!S.activity;
  if (S.activity && !BARE) {
    if (!S.act || !box.contains(S.act.box)) {
      S.act?.destroy();
      const holder = h('div', { class: 'act-box' });
      box.append(holder);
      S.act = { box: holder, ...mountActivity(holder, { appId: S.appId, api, onClose: () => switchView(S.view), openScreen: (slug) => switchView(slug) }) };
    } else { S.act.box.hidden = false; S.act.refresh(); }
  } else if (S.act) { S.act.destroy(); S.act.box.remove(); S.act = null; }
  // An app on its own showing a front page gives the page the whole window:
  // the page has its own header.
  document.body.classList.toggle('front-view', BARE && !!v?.spec?.screen && /"page"\s*:\s*"front"/.test(v.code || ''));
  box.querySelector('.viewerr')?.remove();
  const err = S.errors.get(S.view);
  if (err && S.app?.public) {
    // A visitor of a published app: nothing of how it is made, and nothing
    // that asks the studio (which a visitor may not use), only a way to
    // start the screen again.
    box.append(h('div', { class: 'viewerr' },
      h('div', { class: 'msg' }, h('b', 'Something went wrong on this screen. '), 'Try again; if it keeps happening, tell whoever shared the app.'),
      h('button', { class: 'btn small', onclick: () => { S.frames.get(S.view)?.iframe.remove(); S.frames.delete(S.view); S.errors.delete(S.view); renderFrames(); } }, 'Try again'),
      h('button', { class: 'btn small ghost', onclick: () => { S.errors.delete(S.view); renderFrames(); } }, 'Dismiss')));
  } else if (err) {
    box.append(h('div', { class: 'viewerr' },
      h('div', { class: 'msg' }, h('b', 'This view hit an error. '), err.message, err.repairing ? h('div', { class: 'hint' }, h('span', { class: 'spin' }), ' The engine is repairing it; the fixed version replaces this one when it is ready.') : null),
      !err.repairing ? h('button', { class: 'btn small', onclick: () => ask(`The view "${v?.title}" shows this error: ${err.message}. Fix it.`) }, 'Ask to fix') : null,
      h('button', { class: 'btn small ghost', onclick: () => { S.errors.delete(S.view); renderFrames(); } }, 'Dismiss')));
  }
}

// A build that did not go through, in a plain sentence (the engine's said),
// its error folded under Details.
function failedNote(said, error) {
  const t = said || { title: 'The build did not go through.', hint: '' };
  return [h('div', { class: 'line bad' }, h('b', t.title), t.hint ? ` ${t.hint}` : ''),
    error && t.title !== 'Stopped.' ? h('details', h('summary', { class: 'hint' }, 'Details'), h('div', { class: 'line' }, error)) : null];
}

function buildingScreen() {
  const st = S.app;
  const last = st.requests[0];
  const failed = !st.running && last && ['failed', 'stopped'].includes(last.status) ? last : null;
  return h('div', { class: 'building' }, h('div', { class: 'box' },
    h('h2', failed && !S.lines.length ? 'The first version did not build' : 'Building the first version'),
    h('p', { class: 'hint' }, st.app.description),
    progressBox() || (failed ? h('div', { class: 'log' }, failedNote(failed.said, failed.error), h('button', { class: 'btn small', style: 'align-self:start;margin-top:6px', onclick: () => ask(st.app.description) }, 'Try again')) : h('div', { class: 'hint' }, 'Waiting for the engine...'))));
}

// A build's progress: a bar that moves at the pace builds here usually go
// (each step's expected time, from the last builds' timelines), its steps,
// the pieces being written at once, what it is doing now, and the log.
const STEP_LABELS = {
  read: () => 'Read the app and the library',
  plan: (b) => b.first ? 'Plan the app' : stepKeys(b).includes('write') ? 'Plan the change' : 'Write the change',
  write: (b) => b.first ? 'Write the screens and rules' : 'Write the pieces',
  fix: () => 'Fix what the checks found',
  store: () => 'Check it and store it in ChloeDB',
};

function stepKeys(b) {
  const seen = new Set((b?.steps || []).map((x) => x.key));
  // fix: a round of fixes the checks asked for, counted once it starts
  return ['read', 'plan', 'write', 'fix', 'store'].filter((k) => (k !== 'write' || seen.has(k) || b?.split) && (k !== 'fix' || seen.has(k)));
}

// How far the build is (0..1), how long it has run and about how long is
// left, now: each finished step counts its expected time; inside the current
// one, time goes linearly up to 85% of its share at its expected time and
// then slows, so a long step never fills the bar.
function buildPace(b) {
  const elapsed = (b.elapsed || 0) + (Date.now() - b.at);
  const keys = stepKeys(b);
  const exp = (k) => Math.max(200, b.expect?.[k] || (k === 'write' ? 9000 : k === 'plan' || k === 'fix' ? 15000 : 1500));
  const total = keys.reduce((a, k) => a + exp(k), 0);
  const cur = b.steps.at(-1);
  if (!cur) return { f: 0.01, elapsed, left: total, slow: false, late: elapsed > 2 * total };
  const i = Math.max(0, keys.indexOf(cur.key));
  const before = keys.slice(0, i).reduce((a, k) => a + exp(k), 0);
  const after = keys.slice(i + 1).reduce((a, k) => a + exp(k), 0);
  const e = exp(cur.key);
  const t = Math.max(0, elapsed - cur.t);
  let within = t < e ? 0.85 * t / e : 0.85 + 0.15 * (1 - Math.exp(-(t - e) / e));
  if (cur.key === 'write' && b.pieces?.length) within = Math.max(within, b.pieces.filter((p) => p.state === 'done').length / b.pieces.length * 0.95);
  const f = Math.min(0.97, (before + within * e) / total);
  // late: twice what the whole build usually takes (offer to start it again)
  return { f, elapsed, left: Math.max(0, e - t) + after, slow: t > e * 1.6, late: elapsed > 2 * total };
}

const bsecs = (ms) => ms < 950 ? 'under 1 s' : ms < 10000 ? `${Math.max(1, Math.round(ms / 1000))} s` : ms < 90000 ? `${Math.round(ms / 5000) * 5} s` : `${Math.round(ms / 60000)} min`;

function paceText(b) {
  if (!b) return 'Waiting for its turn';
  const p = buildPace(b);
  return `${bsecs(p.elapsed)} so far · ${p.slow ? 'taking longer than usual' : p.left < 2000 ? 'almost done' : `about ${bsecs(p.left)} left`}`;
}

function progressBox() {
  if (!S.lines.length) return null;
  const b = S.build;
  const text = S.runningText.length > 160 ? `${S.runningText.slice(0, 160)}...` : S.runningText;
  const pct = b ? Math.round(buildPace(b).f * 1000) / 10 : 1;
  const now = S.lines.at(-1);
  const guess = { first: !S.app?.version?.n, split: !S.app?.version?.n };
  const keys = stepKeys(b || guess);
  const at = new Map((b?.steps || []).map((x, i, all) => [x.key, { t: x.t, end: all[i + 1]?.t }]));
  const cur = b?.steps.at(-1)?.key;
  const steps = h('ol', { class: 'bsteps' }, keys.map((k) => {
    const s = at.get(k);
    const state = !s ? 'todo' : k === cur ? 'now' : 'done';
    return h('li', { class: state }, h('span', { class: 'bmark' }, state === 'done' ? '✓' : state === 'now' ? h('span', { class: 'spin' }) : ''),
      h('span', { class: 'what' }, STEP_LABELS[k](b || guess)),
      state === 'done' && s.end != null ? h('span', { class: 'took' }, bsecs(s.end - s.t)) : null);
  }));
  const pieces = b?.pieces?.length ? h('div', { class: 'bpieces' }, b.pieces.map((p) =>
    h('span', { class: `bpiece ${p.state}`, title: `${p.kind} ${p.label}` },
      p.state === 'done' ? h('span', { class: 'bmark' }, '✓') : h('span', { class: 'spin' }),
      h('span', { class: 'what' }, p.label),
      h('span', { class: 'took' }, p.state === 'done' ? `${p.secs} s` : p.chars >= 1000 ? `${(p.chars / 1000).toFixed(1)}k chars` : p.chars ? `${p.chars} chars` : p.state === 'again' ? 'fixing' : '')))) : null;
  const log = h('details', { class: 'blog', ontoggle: (e) => { S.logOpen = e.target.open; } },
    h('summary', `Log (${S.lines.length} ${S.lines.length === 1 ? 'line' : 'lines'})`),
    h('div', { class: 'log' }, S.lines.map((l, i) => h('div', { class: `line${i === S.lines.length - 1 ? ' last' : ''}${/^Failed/.test(l) ? ' bad' : ''}` }, l))));
  log.open = S.logOpen;
  return h('div', { class: 'bprog', id: 'progress' },
    h('div', { class: 'req' }, text),
    h('div', { class: 'bbar', role: 'progressbar', 'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': String(Math.round(pct)) }, h('div', { class: 'fill', style: `width:${pct}%` })),
    h('div', { class: 'bmeta' }, paceText(b)),
    steps, pieces,
    h('div', { class: `bnow${/^Failed/.test(now) ? ' bad' : ''}` }, now),
    BARE ? null : h('div', { class: 'row', style: 'display:flex;gap:6px' },
      h('button', { class: 'btn small ghost', title: 'Stop this build: the model stops at once and the app stays as it is', onclick: () => stopBuild(false) }, 'Stop'),
      b && buildPace(b).late ? h('button', { class: 'btn small', title: 'It is taking twice as long as builds here usually do', onclick: () => stopBuild(true) }, 'Stop and try again') : null),
    log);
}

async function stopBuild(again) {
  const text = S.runningText;
  try {
    const out = await api('POST', `/api/apps/${S.appId}/stop`, S.runningId ? { request: S.runningId } : {});
    if (!out.stopped) { toast('Nothing is being built right now'); return; }
    if (again && text) await ask(text);
  } catch (e) { toast(e.message); }
}

// Moves the bars and the times between the engine's reports.
setInterval(() => {
  if (!S.lines.length) return;
  const b = S.build;
  const pct = b ? Math.round(buildPace(b).f * 1000) / 10 : 1;
  for (const el of document.querySelectorAll('.bbar')) { el.firstChild.style.width = `${pct}%`; el.setAttribute('aria-valuenow', String(Math.round(pct))); }
  for (const el of document.querySelectorAll('.bmeta')) el.textContent = paceText(b);
}, 250);

function renderProgress() {
  if (!S.app) return;
  if (!S.app.views.length) { renderFrames(); }
  if (S.panel === 'change') renderPanel();
}

async function ask(text, extra = {}) {
  await api('POST', `/api/apps/${S.appId}/requests`, { text, ...extra });
  S.failed = null;
  S.runningText = text;
  S.lines = ['Queued'];
  S.build = null;
  S.runningId = null;
  renderProgress();
}

function renderPanel() {
  const st = S.app;
  const panel = $('#panel');
  const active = document.activeElement;
  const draft = panel.querySelector('textarea')?.value || '';
  const hadFocus = active && panel.contains(active) && active.tagName === 'TEXTAREA';
  const tab = (key, label, count) => h('button', { class: `ptab${S.panel === key ? ' on' : ''}`, onclick: () => { S.panel = key; renderPanel(); if (key !== 'change') refreshApp(); } }, label, count ? h('span', { class: 'count' }, count) : null);
  const body = h('div', { class: 'pbody' });
  const screenView = st.views.find((v) => v.slug === S.view && v.spec?.screen);
  if (S.panel === 'edit' && !screenView) S.panel = 'change';
  // The Design tab's previews are live frames: redrawn only when what they
  // show changes, not on every refresh.
  if (S.panel === 'design') {
    const v = st.views.find((x) => x.slug === S.view);
    const key = JSON.stringify([st.design?.designs.map((x) => x.name), v?.id, (st.components || []).map((c) => c.id), st.views.map((w) => w.title)]);
    if (S.designKey === key && panel.querySelector('.dcards')) { markChosen(); return; }
    S.designKey = key;
  } else S.designKey = '';
  if (S.panel === 'change') body.append(...changeTab(draft));
  else if (S.panel === 'edit') body.append(...editTab(screenView));
  else if (S.panel === 'versions') body.append(...versionsTab());
  else if (S.panel === 'design') body.append(...designTab());
  else body.append(...insideTab());
  panel.replaceChildren(h('div', { class: 'ptabs' }, tab('change', 'Change'), screenView ? tab('edit', 'Edit') : null, tab('design', 'Design'), tab('versions', 'Versions', st.versions.length), tab('inside', 'Inside', st.views.length + st.functions.length + st.tables.length),
    h('button', { class: 'btn small ghost mobile-only', style: 'margin-left:auto', onclick: togglePanel }, 'Close')), body);
  if (hadFocus) { const t = panel.querySelector('textarea'); t?.focus(); t?.setSelectionRange(t.value.length, t.value.length); }
}

function changeTab(draft) {
  const st = S.app;
  const firstFailed = !st.version.n && !S.lines.length && ['failed', 'stopped'].includes(st.requests?.[0]?.status);
  const ta = h('textarea', { placeholder: st.version.n ? 'Ask for any change: "add a due date and sort by it", "show a chart of this week", "let me import a list"...'
    : firstFailed ? 'Describe the app again, in other words if you like, to build its first version...' : 'Waiting for the first version...', value: draft });
  const send = h('button', { class: 'btn primary', onclick: submit }, 'Send');
  async function submit() {
    const text = ta.value.trim();
    if (!text) return;
    send.disabled = true;
    ta.value = '';
    try { await ask(text); } catch (e) { toast(e.message); } finally { send.disabled = false; }
  }
  ta.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) submit(); });
  const out = [h('div', { class: 'ask' }, ta, h('div', { class: 'bar' }, send, h('span', { class: 'hint' }, S.lines.length ? 'Requests wait their turn.' : '⌘/Ctrl + Enter')))];
  const prog = progressBox();
  if (prog) out.push(prog);
  else if (S.failed) {
    out.push(h('div', { class: 'log' }, h('div', { class: 'req' }, 'This change did not go through'), failedNote(S.failed.said, S.failed.error),
      h('div', { class: 'row', style: 'display:flex;gap:6px;margin-top:6px' }, h('button', { class: 'btn small', onclick: () => ask(S.failed.text) }, 'Try again'), h('button', { class: 'btn small ghost', onclick: () => { S.failed = null; renderPanel(); } }, 'Dismiss'))));
  } else if (st.version.n && st.version.summary && !st.version.trying) {
    out.push(h('div', { class: 'where' }, h('b', `Version ${st.version.n}: `), st.version.summary));
  }
  // Model and web: in the Change tab once the app uses one or has one on.
  const r = st.reach;
  if (st.version.n && r && (Object.keys(r.abilities || {}).some((k) => r.uses?.[k] || r.on?.[k]) || r.secrets?.length || Object.keys(r.asked || {}).length)) out.push(reachBox(st));
  if (st.version.n && st.evolve) out.push(evolveBox(st));
  if (st.version.n) {
    const sugBtn = h('button', { class: 'btn small ghost', disabled: S.suggesting, onclick: async () => {
      S.suggesting = true; renderPanel();
      try {
        const r = await api('POST', `/api/apps/${S.appId}/suggest`);
        if (r.building) toast(`Building "${r.building.text}" to try, from your use`);
        else if (!r.suggestions.length) toast('Nothing in your use points anywhere yet: use the app a little more');
      } catch (e) { toast(e.message); }
      S.suggesting = false; refreshApp();
    } }, S.suggesting ? h('span', null, h('span', { class: 'spin' }), ' Reading your use...') : 'Suggest from my use');
    out.push(h('div', { class: 'col', style: 'display:flex;flex-direction:column;gap:8px' },
      h('div', { style: 'display:flex;align-items:center;gap:8px' }, h('h2', { class: 'label' }, 'Suggested next'), h('span', { style: 'margin-left:auto' }, sugBtn)),
      st.suggestions.length ? st.suggestions.map((s) => h('div', { class: 'sugg' },
        h('div', { class: 't' }, s.text),
        s.why ? h('div', { class: 'why' }, s.why) : null,
        h('div', { class: 'bar' },
          h('button', { class: 'btn small primary', onclick: async (e) => { e.target.disabled = true; await ask(s.text, { source: 'suggestion', suggestion: s.id }); refreshApp(); } }, 'Do it'),
          h('button', { class: 'btn small ghost', onclick: async () => { await api('POST', `/api/apps/${S.appId}/suggestions/${s.id}`, { status: 'dismissed' }); refreshApp(); } }, 'Not now'),
          h('span', { class: 'src' }, s.source === 'use' ? (s.signal ? 'from a pattern in your use' : 'from your use') : s.source === 'component' ? 'newer shared piece' : s.source === 'look' ? 'from the screen check' : 'from the last build'))))
        : h('div', { class: 'hint' }, 'No suggestions open.')));
  }
  return out;
}

// A model and the web for this app (engine/reach.mjs): each off until the
// owner switches it on here; the app's code says which it uses.
function reachBox(st) {
  const r = st.reach;
  const set = async (patch, said) => {
    try { await api('POST', `/api/apps/${S.appId}/reach`, patch); toast(said); } catch (e) { toast(e.message); }
    refreshApp();
  };
  const rows = Object.entries(r.abilities).map(([k, a]) => {
    const used = !!r.uses?.[k];
    const on = !!r.on?.[k];
    return h('label', { class: `reach-row${used && !on ? ' wants' : ''}` },
      h('input', { type: 'checkbox', checked: on, onchange: (e) => set({ [k]: e.target.checked }, `${a.title} ${e.target.checked ? 'switched on' : 'switched off'} for this app`) }),
      h('span', { class: 'grow' }, h('b', a.title), h('span', { class: 'what' }, a.what)),
      h('span', { class: `pill${used ? (on ? ' ok' : ' warn') : ''}` }, used ? (on ? 'in use' : 'used, off') : 'not used'));
  });
  const t = r.today || {};
  const dollars = (n) => '$' + (n || 0).toFixed(3);
  const perDay = h('select', { class: 'evmode', title: 'How many questions the app may send the model each day (kept answers are free)', onchange: (e) => set({ perDay: Number(e.target.value) }, `At most ${e.target.value} questions to the model a day`) },
    [...new Set([20, 50, 200, 1000, r.on.perDay])].sort((a, b) => a - b).map((n) => h('option', { value: n, selected: n === r.on.perDay }, `${n} a day`)));
  return h('div', { class: 'col reach', style: 'display:flex;flex-direction:column;gap:8px' },
    h('div', { style: 'display:flex;align-items:center;gap:8px' }, h('h2', { class: 'label' }, 'Model and web'), h('span', { style: 'margin-left:auto' }, perDay)),
    ...rows,
    (r.on?.fetch || r.uses?.fetch || r.on?.hosts?.length) ? hostsBox(r, set) : null,
    (r.secrets?.length || r.uses?.secrets?.length || r.on?.fetch) ? secretsBox(r) : null,
    r.schedules?.length ? schedulesBox(r) : null,
    h('div', { class: 'hint' }, `Today: ${t.asks || 0} question${t.asks === 1 ? '' : 's'} to the model${t.cached ? ` and ${t.cached} answered from kept answers` : ''}, ${dollars(t.usd)}; ${t.fetches || 0} page${t.fetches === 1 ? '' : 's'} fetched. Model calls count in this studio's spending limit.`));
}

// The hosts the app may read from, and send to: allowed by the owner one by
// one; those its code names, or tried and was refused, are offered.
function hostsBox(r, set) {
  const allowed = r.on?.hosts || [];
  const waiting = new Map();
  for (const host of r.wants?.hosts || []) waiting.set(host, { send: false, why: 'named in the code' });
  for (const [host, a] of Object.entries(r.asked || {})) waiting.set(host, { send: !!a.send, why: `tried ${a.count} time${a.count === 1 ? '' : 's'}${a.send ? ' to send' : ''}` });
  const input = h('input', { class: 'inp small', placeholder: 'api.example.com', style: 'flex:1;min-width:0' });
  const add = () => { const v = input.value.trim(); if (v) set({ allow: { host: v, send: false } }, `${v} allowed to read from`); };
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter') add(); });
  return h('div', { class: 'reach-sub' },
    h('div', { class: 'reach-head' }, h('b', 'Hosts'), h('span', { class: 'what' }, 'The app reads only from these, and sends (POST) only to those marked. A host covers its subdomains.')),
    ...allowed.map((x) => h('div', { class: 'reach-item' },
      h('span', { class: 'grow' }, x.host === '*' ? h('span', null, h('b', 'Any public host'), h('span', { class: 'what' }, ' (switched on before host lists: replace it with the hosts the app needs)')) : h('b', x.host)),
      h('label', { class: `send${x.send ? ' on' : ''}`, title: 'May the app send data to this host (POST)?' },
        h('input', { type: 'checkbox', checked: !!x.send, onchange: (e) => set({ allow: { host: x.host, send: e.target.checked } }, `${x.host === '*' ? 'Any host' : x.host}: ${e.target.checked ? 'reads and sends' : 'reads only'}`) }), ' sends'),
      h('button', { class: 'btn small ghost', onclick: () => set({ forget: x.host }, `${x.host === '*' ? 'Any host' : x.host} no longer allowed`) }, 'Remove'))),
    ...[...waiting].filter(([host, w]) => !allowed.some((x) => (x.host === '*' || host === x.host || host.endsWith(`.${x.host}`)) && (!w.send || x.send))).map(([host, w]) => h('div', { class: 'reach-item wants' },
      h('span', { class: 'grow' }, h('b', host), h('span', { class: 'what' }, ` ${w.why}, not allowed`)),
      h('button', { class: 'btn small', onclick: () => set({ allow: { host, send: w.send } }, `${host} allowed to ${w.send ? 'read from and send to' : 'read from'}`) }, w.send ? 'Allow reading and sending' : 'Allow'))),
    !allowed.length && !waiting.size ? h('div', { class: 'hint' }, 'No hosts allowed yet: the app reaches none.') : null,
    h('div', { class: 'reach-item' }, input, h('button', { class: 'btn small', onclick: add }, 'Allow host')));
}

// API keys: pasted once, sealed by the engine, sent only to their hosts,
// never shown again.
function secretsBox(r) {
  const save = async (body, said) => {
    try { await api('POST', `/api/apps/${S.appId}/secrets`, body); toast(said); } catch (e) { toast(e.message); }
    refreshApp();
  };
  const have = new Set((r.secrets || []).map((x) => x.name));
  const missing = (r.uses?.secrets || []).filter((n) => !have.has(n));
  const f = {
    name: h('input', { class: 'inp small', placeholder: 'name (as the code says)', value: missing[0] || '', style: 'flex:1 1 160px;min-width:120px' }),
    value: h('input', { class: 'inp small', type: 'password', autocomplete: 'off', placeholder: 'the key', style: 'flex:2 1 220px;min-width:160px' }),
    hosts: h('input', { class: 'inp small', placeholder: 'sent only to: api.example.com', value: (r.wants?.hosts || r.uses?.hosts || [])[0] || '', style: 'flex:2 1 220px;min-width:160px' }),
    where: h('select', { class: 'evmode' }, h('option', { value: 'bearer' }, 'Authorization: Bearer'), h('option', { value: 'header' }, 'a header...'), h('option', { value: 'param' }, 'a query parameter...')),
    pname: h('input', { class: 'inp small', placeholder: 'x-api-key or key', style: 'width:130px;display:none' }),
  };
  f.where.addEventListener('change', () => { f.pname.style.display = f.where.value === 'bearer' ? 'none' : ''; });
  const add = () => {
    const where = f.where.value;
    const body = { name: f.name.value.trim(), value: f.value.value, hosts: f.hosts.value };
    if (where === 'header') Object.assign(body, { header: f.pname.value.trim(), prefix: '' });
    if (where === 'param') body.param = f.pname.value.trim();
    save(body, `Secret ${body.name} kept; sent only to ${body.hosts}`);
  };
  return h('div', { class: 'reach-sub' },
    h('div', { class: 'reach-head' }, h('b', 'Secrets'), h('span', { class: 'what' }, 'API keys the code names as {secret: \'name\'}. Kept sealed by the engine, sent only to their hosts, never shown again.')),
    ...(r.secrets || []).map((x) => h('div', { class: 'reach-item' },
      h('span', { class: 'grow' }, h('b', x.name), h('span', { class: 'what' }, ` ${x.hint ? `${x.hint} · ` : ''}to ${x.hosts.join(', ')} · ${x.param ? `?${x.param}=` : `${x.header}: ${x.prefix || ''}…`} · ${x.used ? `used ${new Date(x.used).toLocaleString()}` : 'not used yet'}`)),
      h('button', { class: 'btn small ghost', onclick: async () => { try { await api('POST', `/api/apps/${S.appId}/secrets/${x.name}/delete`); toast(`Secret ${x.name} removed`); } catch (e) { toast(e.message); } refreshApp(); } }, 'Remove'))),
    ...missing.map((n) => h('div', { class: 'reach-item wants' }, h('span', { class: 'grow' }, h('b', n), h('span', { class: 'what' }, ' the code asks for it; not set yet')))),
    h('div', { class: 'reach-item', style: 'flex-wrap:wrap' }, f.name, f.value, f.hosts, f.where, f.pname, h('button', { class: 'btn small', onclick: add }, 'Keep secret')));
}

// Rules and functions that run on a clock: when, how the last run went.
function schedulesBox(r) {
  const act = async (slug, what, said) => {
    try { await api('POST', `/api/apps/${S.appId}/schedules/${slug}/${what}`); toast(said); } catch (e) { toast(e.message); }
    refreshApp();
  };
  const when = (iso) => (iso ? new Date(iso).toLocaleString() : '');
  return h('div', { class: 'reach-sub' },
    h('div', { class: 'reach-head' }, h('b', 'Schedules'), h('span', { class: 'what' }, r.on?.schedule ? 'They run on their own, with nobody watching; a schedule stops after 5 failures in a row.' : 'Switch on "Schedules" above for these to run on their own.')),
    ...r.schedules.map((x) => h('div', { class: `reach-item${x.status === 'failed' || x.paused ? ' wants' : ''}` },
      h('span', { class: 'grow' },
        h('b', `${x.title}${x.source ? ' (source)' : ''}`),
        h('span', { class: 'what' }, ` ${x.when}${x.next ? ` · next ${when(typeof x.next === 'number' ? new Date(x.next).toISOString() : x.next)}` : x.paused ? ' · paused' : ''}`),
        x.last ? h('span', { class: 'what' }, ` · last ${when(x.last)}: ${x.status}${x.error ? `: ${x.error}` : x.result ? `: ${x.result}` : ''}`) : null),
      h('button', { class: 'btn small', onclick: () => act(x.slug, 'run', `${x.title} ran`) }, 'Run now'),
      h('button', { class: 'btn small ghost', onclick: () => act(x.slug, x.paused ? 'resume' : 'pause', `${x.title} ${x.paused ? 'started again' : 'paused'}`) }, x.paused ? 'Start again' : 'Pause'))));
}

// Built from use: the changes the engine made on its own from patterns in
// what people do, to try, keep or drop; and the patterns it sees.
function evolveBox(st) {
  const ev = st.evolve;
  const STATUS = { building: 'building', ready: 'ready to try', failed: 'did not build', kept: 'kept', dropped: 'dropped' };
  const effect = (c) => {
    const e = c.effect;
    if (!e || !c.measure) return null;
    const span = (d) => (d < 1 ? `${Math.max(1, Math.round(d * 24))} h` : `${Math.round(d)} day${Math.round(d) === 1 ? '' : 's'}`);
    if (e.ux) {
      const val = (x) => (x === null || x === undefined ? 'not measured' : /^(first|gaveUp|wrong|undo)$/.test(e.ux) ? `${Math.round(x * 100)}%` : String(Math.round(x * 10) / 10));
      return h('div', { class: 'effect' }, h('b', 'Since kept: '), `${c.measure.says || 'measure'}: ${val(e.before)} over ${e.sittingsBefore} visits before, ${val(e.after)} over ${e.sittingsAfter} since`);
    }
    return h('div', { class: 'effect' }, h('b', 'Since kept: '), `${c.measure.says || 'measure'}: ${e.before} in the ${span(e.beforeDays)} before, ${e.after} in the ${span(e.afterDays)} since`);
  };
  const cards = (ev.candidates || []).filter((c) => c.status !== 'dropped').slice(0, 4).map((c) => h('div', { class: `sugg cand ${c.status}` },
    h('div', { class: 't' }, c.text),
    c.why ? h('div', { class: 'why' }, c.why) : null,
    c.status === 'ready' && c.summary ? h('div', { class: 'why' }, h('b', `Version ${c.n}: `), c.summary) : null,
    c.problems?.length ? h('div', { class: 'why bad' }, c.problems.slice(0, 2).join('; ')) : null,
    c.status === 'kept' ? effect(c) : null,
    h('div', { class: 'bar' },
      c.status === 'ready' ? h('button', { class: `btn small${st.version.trying === c.n ? '' : ' primary'}`, onclick: () => tryVersion(st.version.trying === c.n ? null : c.n) }, st.version.trying === c.n ? 'Back to live' : 'Try it') : null,
      c.status === 'ready' ? h('button', { class: 'btn small', onclick: () => decide(c, 'keep') }, 'Keep') : null,
      c.status === 'ready' || c.status === 'failed' ? h('button', { class: 'btn small ghost', onclick: () => decide(c, 'drop') }, c.status === 'failed' ? 'Dismiss' : 'Drop') : null,
      c.status === 'building' ? h('span', { class: 'hint' }, h('span', { class: 'spin' }), ' Building beside the live version...') : null,
      h('span', { class: `src st-${c.status}` }, STATUS[c.status] || c.status))));
  const mode = h('select', { class: 'evmode', title: 'What the engine does with patterns in how people use this app', onchange: async (e) => {
    try { await api('POST', `/api/apps/${S.appId}/evolve`, { mode: e.target.value }); toast(e.target.value === 'build' ? 'The engine will build changes from use for you to try' : e.target.value === 'suggest' ? 'The engine will only suggest' : 'The engine will not look at use'); } catch (err) { toast(err.message); }
    refreshApp();
  } }, [['build', 'Build to try'], ['suggest', 'Suggest only'], ['off', 'Off']].map(([v, l]) => h('option', { value: v, selected: ev.mode === v }, l)));
  const sigs = (ev.signals || []).slice(0, 6);
  return h('div', { class: 'col evolve', style: 'display:flex;flex-direction:column;gap:8px' },
    h('div', { style: 'display:flex;align-items:center;gap:8px' }, h('h2', { class: 'label' }, 'Built from use'), h('span', { style: 'margin-left:auto' }, mode)),
    cards.length ? cards : h('div', { class: 'hint' }, ev.mode === 'build'
      ? 'When a pattern shows in how people use the app (a search typed again and again, a filter set every visit, the same field changed row after row), the engine builds a change that answers it, beside the live version, for you to try.'
      : ev.mode === 'suggest' ? 'Patterns in use become suggestions below; nothing is built on its own.' : 'Off: the engine does not look at how the app is used.'),
    sigs.length ? h('details', { class: 'signals' }, h('summary', null, `Patterns in use (${sigs.length})`),
      sigs.map((x) => h('div', { class: 'signal' }, h('span', { class: 'grow' }, x.text), x.answered ? h('span', { class: 'pill' }, 'answered') : null))) : null);
}

function versionsTab() {
  const st = S.app;
  const head = st.app.head;
  // The request that made each version: its timeline is kept by request.
  const builtBy = new Map(st.requests.filter((r) => r.status === 'done' && Number(r.comp)).map((r) => [Number(r.comp), r]));
  const items = st.versions.map((c) => h('div', { class: `ver${Number(c.n) === Number(head) ? ' on' : ''}` },
    h('div', { class: 'n' }, `v${c.n}`),
    h('div', { class: 'body' },
      h('div', c.summary || '(no summary)'),
      c.request ? h('div', { class: 'req' }, `“${c.request}”`) : null,
      h('div', { class: 'meta' }, h('span', ago(c.created)), h('span', `${(c.caps || []).length} capabilities`),
        builtBy.get(Number(c.n)) ? h('button', { class: 'btn small ghost', title: 'what took the time, and what was used, rewritten or written new', onclick: () => showTimeline(builtBy.get(Number(c.n)).id, `How version ${c.n} was built`) }, 'Timeline') : null,
        Number(c.n) === Number(head) ? h('span', { class: 'pill' }, 'live') : h('button', { class: 'btn small', onclick: async () => { await api('POST', `/api/apps/${S.appId}/restore`, { n: c.n }); toast(`Back to version ${c.n}`); refreshApp(); } }, 'Go back to this')))));
  const failed = st.requests.filter((r) => r.status === 'failed').slice(0, 3);
  const stats = h('div', { class: 'tl-stats' });
  buildStats(stats);
  return [
    h('div', { class: 'where' }, 'Each version is a row of sa_compositions in ChloeDB naming the capabilities it is made of. Going back moves the app to that row; data stays as it is (tables only grow).'),
    stats,
    h('div', { class: 'versions' }, items.length ? items : h('div', { class: 'hint' }, 'No version yet.')),
    failed.length ? h('div', { class: 'col', style: 'display:flex;flex-direction:column;gap:6px' }, h('h2', { class: 'label' }, 'Requests that failed'),
      failed.map((r) => h('div', { class: 'sugg' }, h('div', { class: 'why' }, `“${r.text}”`), h('div', { class: 'small err' }, r.error),
        h('div', null, h('button', { class: 'btn small ghost', onclick: () => showTimeline(r.id, 'How this build failed') }, 'Timeline'))))) : null,
  ].filter(Boolean);
}

// A screen written as a CREATE SCREEN statement is edited as its statement:
// the engine compiles it, checks it, runs its queries and stores it.
function statementTab(view) {
  const source = view.sql || view.spec.statement;
  const box = h('textarea', { class: 'statement', spellcheck: 'false', rows: Math.min(30, source.split('\n').length + 2) });
  const err = h('div', { class: 'statement-error' });
  box.value = source;
  const save = h('button', { class: 'btn primary', onclick: async () => {
    if (S.app.running) { toast('A change is being built: edit once it is live'); return; }
    S.editing = true;
    save.disabled = true;
    err.textContent = '';
    try {
      const r = await api('POST', `/api/apps/${S.appId}/screens/${view.slug}`, { statement: box.value, note: `Edited the statement of ${view.title} by hand` });
      if (r.unchanged) toast('Nothing changed');
    } catch (e) { err.textContent = e.message; }
    save.disabled = false;
    S.editing = false;
  } }, 'Save');
  return [
    h('div', { class: 'where' }, 'This screen is a CREATE SCREEN statement. Edit it and save: no model, compiled, checked and stored at once as a new version (Versions goes back).'),
    box,
    err,
    h('div', { style: 'display:flex; gap:8px; margin-top:8px' }, save, h('button', { class: 'btn ghost', onclick: () => { box.value = source; } }, 'Undo my edits')),
  ];
}

// Edits of a screen by hand: move, hide or rename its parts, and reorder,
// rename or hide a table's columns and a form's fields. Each edit is a JSON
// Patch the engine checks, runs and stores as a new version, with no model.
function editTab(view) {
  if (view.spec?.statement) return statementTab(view);
  let screen;
  try { screen = JSON.parse(view.code); } catch { return [h('div', { class: 'hint' }, 'This screen cannot be read.')]; }
  const send = async (patch, note) => {
    if (S.app.running) { toast('A change is being built: edit once it is live'); return; }
    S.editing = true;
    try { const r = await api('POST', `/api/apps/${S.appId}/screens/${view.slug}`, { patch, note }); if (r.unchanged) toast('Nothing changed'); } catch (e) { toast(e.message); }
    S.editing = false;
  };
  const isText = (v) => typeof v === 'string' && !v.startsWith('=');
  const KINDS = ['h', 'text', 'ui', 'row', 'col', 'grid', 'split', 'card', 'section', 'if', 'each', 'kpis', 'chips', 'input', 'button', 'form', 'table', 'code'];
  const HOLD = ['row', 'col', 'grid', 'split', 'card', 'section', 'then', 'else', 'body'];
  const NAMES = { h: 'Heading', text: 'Text', ui: 'Component', row: 'Row', col: 'Column', grid: 'Grid', split: 'Two columns', card: 'Card', section: 'Section', if: 'Shown when', each: 'List', kpis: 'Big numbers', chips: 'Filter chips', input: 'Input', button: 'Button', form: 'Form', table: 'Table', code: 'Code' };
  // The text a person may rename in a block, by the key that holds it.
  const textKey = (b) => ['h', 'text', 'button', 'title', 'placeholder', 'empty', 'label'].find((k) => isText(b[k]));
  const out = [h('div', { class: 'where' }, 'Change this screen by hand: no model, stored at once as a new version (Versions goes back). For anything else, ask in Change.')];
  const rows = [];
  const walk = (list, path, depth) => {
    (list || []).forEach((b, i) => {
      const at = `${path}/${i}`;
      if (!b || typeof b !== 'object') { rows.push({ at, depth, b: { text: String(b) }, kind: 'text', list, i, path }); return; }
      const kind = KINDS.find((k) => b[k] !== undefined) || '?';
      rows.push({ at, depth, b, kind, list, i, path });
      if (depth < 3) for (const k of HOLD) if (Array.isArray(b[k])) walk(b[k], `${at}/${k}`, depth + 1);
    });
  };
  walk(screen.body, '/body', 0);
  const box = h('div', { class: 'caps' });
  for (const r of rows) {
    const tk = textKey(r.b);
    const name = h('span', { class: 'pill' }, r.kind === 'ui' ? `ui.${r.b.ui}` : NAMES[r.kind] || r.kind);
    const label = tk ? h('input', { value: r.b[tk], style: 'flex:1;min-width:0', title: `Rename (${tk})`, onchange: (e) => send([{ op: 'replace', path: `${r.at}/${tk}`, value: e.target.value }], `Renamed "${r.b[tk]}" to "${e.target.value}"`) })
      : h('span', { class: 't' }, r.kind === 'table' || r.kind === 'each' ? 'rows' : r.kind === 'form' ? `adds to ${r.b.form}` : '');
    const btn = (text, title, patch, note, disabled) => h('button', { class: 'btn small ghost', title, disabled: disabled || S.editing, onclick: () => send(patch, note) }, text);
    const what = `${NAMES[r.kind] || r.kind}${tk ? ` "${r.b[tk]}"` : ''}`;
    const line = h('div', { class: 'cap', style: `cursor:default;margin-left:${r.depth * 14}px` }, name, label,
      btn('↑', 'Move up', [{ op: 'move', from: r.at, path: `${r.path}/${r.i - 1}` }], `Moved ${what} up`, r.i === 0),
      btn('↓', 'Move down', [{ op: 'move', from: r.at, path: `${r.path}/${r.i + 1}` }], `Moved ${what} down`, r.i === r.list.length - 1),
      btn('Hide', 'Take it out of the screen', [{ op: 'remove', path: r.at }], `Took out ${what}`));
    box.append(line);
    // A table's columns and a form's fields, one chip each.
    const items = r.kind === 'table' && Array.isArray(r.b.columns) ? ['columns', r.b.columns, (c) => c.label || c.key] : r.kind === 'form' && Array.isArray(r.b.fields) ? ['fields', r.b.fields, (f) => (typeof f === 'string' ? f : f.label || f.name)] : null;
    if (items) {
      const [key, list, show] = items;
      box.append(h('div', { style: `display:flex;flex-wrap:wrap;gap:6px;margin-left:${r.depth * 14 + 14}px` }, list.map((c, k) => {
        const obj = typeof c === 'object' && c;
        const text = show(c);
        return h('span', { class: 'chip', style: 'display:inline-flex;gap:4px;align-items:center;padding:2px 6px;border:1px solid var(--line);border-radius:999px;font-size:12px' },
          k > 0 ? h('button', { class: 'btn small ghost', style: 'padding:0 4px', title: 'Move left', onclick: () => send([{ op: 'move', from: `${r.at}/${key}/${k}`, path: `${r.at}/${key}/${k - 1}` }], `Moved ${text} left`) }, '‹') : null,
          obj ? h('input', { value: String(text), style: 'width:9em;font-size:12px;padding:1px 4px', title: 'Rename', onchange: (e) => send([{ op: 'add', path: `${r.at}/${key}/${k}/label`, value: e.target.value }], `Renamed ${text} to ${e.target.value}`) }) : h('span', null, String(text)),
          k < list.length - 1 ? h('button', { class: 'btn small ghost', style: 'padding:0 4px', title: 'Move right', onclick: () => send([{ op: 'move', from: `${r.at}/${key}/${k}`, path: `${r.at}/${key}/${k + 1}` }], `Moved ${text} right`) }, '›') : null,
          h('button', { class: 'btn small ghost', style: 'padding:0 4px', title: 'Hide', onclick: () => send([{ op: 'remove', path: `${r.at}/${key}/${k}` }], `Took out the ${key === 'columns' ? 'column' : 'field'} ${text}`) }, '×'));
      })));
    }
  }
  out.push(box);
  return out;
}

function insideTab() {
  const st = S.app;
  const cap = (c, extra) => h('div', { class: 'cap', onclick: () => showCode(c.id) },
    h('span', { class: `pill ${c.kind}` }, c.kind), h('span', { class: 't' }, c.title || c.slug),
    c.author === 'repair' ? h('span', { class: 'pill repair' }, 'repaired') : null,
    c.forked_from ? h('span', { class: 'pill library' }, 'library') : null,
    extra ? h('span', { class: 'v' }, extra) : null, h('span', { class: 'v' }, `v${c.version}`));
  return [
    ...(st.version.n && st.reach ? [reachBox(st)] : []),
    h('div', { class: 'where' }, `This app is data in ChloeDB: version ${st.version.n} is the row `, h('span', { class: 'mono' }, `${st.app.id}#${st.version.n}`),
      ' of sa_compositions; each piece below is a row of sa_capabilities, stored as its SQL; the app\'s data is in tables named ', h('span', { class: 'mono' }, `app_${st.app.id}_*`), '. Click a piece to read its SQL.'),
    st.version.n ? h('div', { style: 'display:flex;gap:6px;flex-wrap:wrap' },
      h('button', { class: 'btn small', onclick: () => showAppSql(st.app.id) }, 'Whole app as SQL'),
      h('button', { class: 'btn small', onclick: () => showMap(st.app.id, st.app.name) }, 'Map: how the pieces link')) : null,
    h('h2', { class: 'label' }, 'Views'), h('div', { class: 'caps' }, st.views.map((c) => cap(c))),
    h('h2', { class: 'label' }, 'Components (shared with other apps)'), h('div', { class: 'caps' }, (st.components || []).length ? st.components.map((c) => cap(c,
      [c.users > 1 ? `used by ${c.users} apps` : 'only this app so far', c.latest > c.version ? `v${c.latest} available` : null].filter(Boolean).join(' · '))) : h('div', { class: 'hint' }, 'None')),
    h('h2', { class: 'label' }, 'Helpers (shared with other apps)'), h('div', { class: 'caps' }, (st.helpers || []).length ? st.helpers.map((c) => cap(c,
      [c.users > 1 ? `used by ${c.users} apps` : 'only this app so far', c.latest > c.version ? `v${c.latest} available` : null].filter(Boolean).join(' · '))) : h('div', { class: 'hint' }, 'None')),
    h('h2', { class: 'label' }, 'Functions'), h('div', { class: 'caps' }, st.functions.length ? st.functions.map((c) => cap(c, c.spec?.on ? `on ${[].concat(c.spec.on.event || 'insert').join(' or ')} ${c.spec.on.table}` : null)) : h('div', { class: 'hint' }, 'None')),
    h('h2', { class: 'label' }, 'Tables'), h('div', { class: 'caps' }, st.tables.map((t) => h('div', null,
      h('div', { class: 'cap', onclick: () => showCode(t.id) }, h('span', { class: 'pill table' }, 'table'), h('span', { class: 't' }, t.name), h('span', { class: 'v' }, `${t.rows} rows`)),
      h('div', { class: 'cols' }, (t.columns || []).map((c) => `${c.name} ${c.type}`).join(' · '))))),
    h('h2', { class: 'label' }, `People (${(st.people || []).length})`), h('div', { class: 'caps' }, (st.people || []).length
      ? st.people.map((p) => h('div', { class: 'cap' }, h('span', { class: 'pill person' }, p.role), h('span', { class: 't' }, p.name), h('span', { class: 'v' }, p.email)))
      : h('div', { class: 'hint' }, 'Nobody has signed in yet. People join an app by signing in from one of its views (ui.signIn); the first becomes its owner. Rows of sa_people and sa_members.')),
    h('h2', { class: 'label' }, 'Outbox'), h('div', { class: 'caps' }, (st.outbox || []).length
      ? st.outbox.map((o) => h('div', { class: 'cap', onclick: () => showEmail(o) }, h('span', { class: `pill mail-${o.status}` }, o.status), h('span', { class: 't' }, o.subject || '(no subject)'), h('span', { class: 'v' }, `to ${(o.recipients || []).join(', ')} · ${ago(o.created)}`)))
      : h('div', { class: 'hint' }, 'No email yet. Email the app sends is a row of sa_outbox; it is held here until the engine is given a mail server (SMTP_URL and SMTP_FROM).')),
  ];
}

function showEmail(o) {
  const modal = $('#modal');
  const close = () => { modal.hidden = true; modal.replaceChildren(); };
  modal.onclick = (e) => { if (e.target === modal) close(); };
  modal.replaceChildren(h('div', { class: 'sheet' },
    h('header', h('span', { class: `pill mail-${o.status}` }, o.status), h('span', { class: 't' }, o.subject || '(no subject)'), h('button', { class: 'btn small ghost', onclick: close }, 'Close')),
    h('div', { class: 'info' }, h('div', `To ${(o.recipients || []).join(', ')} · ${ago(o.created)}`),
      o.status === 'held' ? h('div', 'Held: this engine has no mail server, so the message stays here.') : null,
      o.error ? h('div', { class: 'err' }, o.error) : null),
    h('pre', o.body || '')));
  modal.hidden = false;
}

// ---- bridge between view frames and the engine

// A file from a frame goes to the engine as the request's body.
async function upload(base, file) {
  if (!(file instanceof Blob)) throw new Error('app.upload takes a File (from an <input type="file"> or a drop)');
  const q = new URLSearchParams({ name: file.name || 'file', type: file.type || 'application/octet-stream' });
  let res;
  try { res = await fetch(`${base}/files?${q}`, { method: 'POST', headers: { 'content-type': 'application/octet-stream', ...AS_STRANGER }, body: file }); } catch { throw unanswered(); }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw failed(res, data);
  return { ...data.file, url: location.origin + data.file.url };
}

// After someone signs in or out every view starts again, so that each one
// sees who is there now.
function authChanged() {
  setTimeout(() => {
    for (const f of S.frames.values()) f.iframe.remove();
    S.frames.clear();
    S.errors.clear();
    refreshApp();
  }, 400);
}

window.addEventListener('message', async (e) => {
  const m = e.data;
  if (!m || !m.__sa || !S.appId) return;
  let entry = null;
  for (const [slug, f] of S.frames) if (f.iframe.contentWindow === e.source) entry = { slug, ...f };
  if (!entry) {
    // A design's preview reads the app's data and changes none of it.
    if (![...document.querySelectorAll('.dcard iframe')].some((f) => f.contentWindow === e.source) || m.kind !== 'rpc') return;
    if (!['sql', 'me', 'people', 'files'].includes(m.method)) { e.source.postMessage({ __sa: 1, kind: 'result', id: m.id, error: 'a preview changes nothing' }, '*'); return; }
    entry = { slug: 'preview', preview: true };
  }
  const reply = (value, error, refused, transient) => e.source.postMessage({ __sa: 1, kind: 'result', id: m.id, value, error, refused, transient }, '*');
  const base = `/api/apps/${S.appId}`;
  // Writes and calls name the screen they were made on, and the taps (in the
  // app and on its tabs) since the write before.
  if (m.kind === 'tap') { if (!entry.preview) S.taps = (S.taps || 0) + 1; return; }
  const on = entry.preview ? {} : { 'x-sa-view': entry.slug };
  if (!entry.preview && m.kind === 'rpc' && ['insert', 'update', 'remove', 'call'].includes(m.method)) { on['x-sa-taps'] = String(S.taps || 0); S.taps = 0; }
  try {
    if (m.kind === 'rpc') {
      const [a, b, c] = m.args || [];
      let value;
      switch (m.method) {
        case 'sql': value = (await api('POST', `${base}/sql${atV()}`, { sql: a, params: b })).rows; break;
        case 'insert': value = (await api('POST', `${base}/insert${atV()}`, { table: a, row: b }, on)).row; break;
        case 'update': value = (await api('POST', `${base}/update${atV()}`, { table: a, id: b, fields: c }, on)).row; break;
        case 'remove': value = await api('POST', `${base}/remove${atV()}`, { table: a, id: b }, on); break;
        case 'call': value = (await api('POST', `${base}/call${atV()}`, { name: a, input: b }, on)).result; break;
        case 'me': value = (await api('GET', `${base}/me`)).user; break;
        case 'signIn': value = (await api('POST', `${base}/signin`, a)).user; authChanged(); break;
        case 'signUp': value = (await api('POST', `${base}/signup`, a)).user; authChanged(); break;
        case 'signOut': value = await api('POST', '/api/signout'); authChanged(); break;
        case 'people': value = (await api('GET', `${base}/people`)).people; break;
        case 'companies': value = (await api('GET', `${base}/companies`, null, companyHeader())).companies; break;
        case 'company': value = (await api('GET', `${base}/companies`, null, companyHeader())).current; break;
        case 'createCompany': value = (await api('POST', `${base}/companies`, { name: a })).company; S.company = value.id; authChanged(); break;
        case 'useCompany': S.company = a || null; value = (await api('GET', `${base}/companies`, null, companyHeader())).current; authChanged(); break;
        case 'addToCompany': value = (await api('POST', `${base}/companies/members`, { company: a, email: b, role: c || 'member' })).member; break;
        case 'files': value = (await api('POST', `${base}/files/info`, { ids: a })).files.map((f) => ({ ...f, url: location.origin + f.url })); break;
        case 'email': value = (await api('POST', `${base}/email`, a)).email; break;
        case 'geocode': value = (await api('GET', `/api/geocode?q=${encodeURIComponent(a || '')}`)).places; break;
        case 'ask': value = (await api('POST', `${base}/ask`, { prompt: a, opts: b || {} })).answer; break;
        case 'fetch': value = (await api('POST', `${base}/fetch`, { url: a, opts: b || {} })).page; break;
        case 'upload': value = await upload(base, a); break;
        default: throw new Error(`no app.${m.method}`);
      }
      reply(value);
    } else if (m.kind === 'track') {
      const kind = m.action === 'abandon' ? 'abandon' : 'action';
      api('POST', `${base}/track${atV()}`, { kind, action: m.action, detail: m.detail, cap: entry.capId, q: m.q, results: m.results, table: m.table, ms: m.ms }, on).catch(() => {});
    } else if (m.kind === 'link') {
      // The frame has no popups: its links open from here, and only on a
      // click (the browser's popup blocker wants one).
      const url = linkToOpen(m.href);
      if (url) window.open(url, '_blank', 'noopener,noreferrer');
    } else if (m.kind === 'open') {
      if (S.app.views.find((v) => v.slug === m.slug)) switchView(m.slug, m.params);
    } else if (m.kind === 'error') {
      const out = await api('POST', `${base}/error`, { cap: m.cap || entry.capId, message: `${m.message}\n${m.stack || ''}`.slice(0, 1500) }).catch(() => ({}));
      S.errors.set(entry.slug, { message: m.message, repairing: !!out.repairing });
      renderApp();
    }
  } catch (err) {
    if (m.kind === 'rpc') reply(undefined, err.message, !!err.refused, !!err.transient);
  }
});

window.addEventListener('hashchange', route);
window.addEventListener('keydown', (e) => { if (e.key === 'Escape') { const md = $('#modal'); md.hidden = true; md.replaceChildren(); } });
connect();
route();
