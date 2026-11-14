// Screens: views written as data. A screen names its data (SQL against the
// app's tables), its state, and its blocks (headings, forms, tables, lists,
// shared components with their props); this runtime fetches the data, draws
// the blocks, re-runs the queries after every write and redraws when the
// state changes. Values that start with "=" are expressions; the studio
// compiles each one into a function before the frame loads (window.__SA_EXPR,
// window.__SA_CODE), so the frame needs no eval.
(() => {
  const isExpr = (v) => v && typeof v === 'object' && !Array.isArray(v) && typeof v.$e === 'number' && Object.keys(v).length === 1;
  const OPT = (o) => (o && typeof o === 'object' ? { value: o.value ?? o.id ?? o.label, label: o.label ?? o.name ?? String(o.value) } : { value: o, label: String(o) });
  const human = (k) => String(k).replace(/_/g, ' ').replace(/^./, (c) => c.toUpperCase());
  // What a row is called: its name or title, else its first short text. A
  // button repeated on every row says which row it acts on ("Delete: Basil").
  const NAMES = ['name', 'title', 'label', 'subject', 'item', 'task', 'what', 'text'];
  const DAYLIKE = /^\d{4}-\d{2}-\d{2}/;
  const rowName = (r) => {
    if (!r || typeof r !== 'object') return '';
    for (const k of NAMES) if (typeof r[k] === 'string' && r[k].trim()) return r[k].trim().slice(0, 60);
    for (const [k, v] of Object.entries(r)) if (k !== 'id' && !/_id$/.test(k) && typeof v === 'string' && v.trim() && v.length <= 60 && !DAYLIKE.test(v) && !/^(https?:|data:)/.test(v)) return v.trim();
    return '';
  };

  window.__sa_screen = async (root, app, spec) => {
    const E = window.__SA_EXPR || [];
    const SRC = window.__SA_EXPR_SRC || [];
    const CODE = window.__SA_CODE || [];
    let state = {};
    let data = {};
    let lets = {};
    let drawing = false;
    let again = false;
    const forms = new Map();
    const reported = new Set();
    // The design's look (frame.js sets it from the design). Native (the
    // default) says the first big numbers in a sentence and, on a phone,
    // draws a table's rows as list cells grouped by their state; bold draws
    // the numbers as a band and each row as a card; plain as they come.
    const lookIs = () => document.documentElement.dataset.look || 'plain';
    const bold = () => lookIs() === 'bold';
    const phone = window.matchMedia('(max-width: 560px)');
    let heroAt = null;
    // A word straight from the data ("out", "in progress") reads as a label:
    // "Out", "In progress".
    const cap = (t) => (lookIs() === 'plain' || typeof t !== 'string' ? t : /^ok$/i.test(t) ? 'OK' : /^[a-z]/.test(t) && t === t.toLowerCase() ? t[0].toUpperCase() + t.slice(1) : t);
    // How many rows a search or a filter on state[key] found: the first list
    // that depends on it (a LET over the key, else the data it is a parameter
    // of). Nothing when no list depends on it.
    const exprIds = (v, out = []) => {
      if (isExpr(v)) out.push(v.$e);
      else if (Array.isArray(v)) v.forEach((x) => exprIds(x, out));
      else if (v && typeof v === 'object') Object.values(v).forEach((x) => exprIds(x, out));
      return out;
    };
    const mentions = (v, res) => exprIds(v).some((i) => res.some((re) => re.test(String(SRC[i] || ''))));
    const resultsFor = (key) => {
      const k = String(key).replace(/[^A-Za-z0-9_$]/g, '');
      if (!k) return undefined;
      const byKey = [new RegExp(`state\\.${k}\\b|state\\[['"]${k}['"]\\]`)];
      const dataDeps = Object.entries(spec.data || {}).filter(([, q]) => mentions(q, byKey)).map(([n]) => n);
      const byData = dataDeps.map((n) => new RegExp(`\\b${n.replace(/[^A-Za-z0-9_$]/g, '')}\\b`));
      const direct = Object.entries(spec.let || {}).filter(([, v]) => mentions(v, byKey)).map(([n]) => lets[n]);
      const over = Object.entries(spec.let || {}).filter(([, v]) => byData.length && mentions(v, byData)).map(([n]) => lets[n]);
      const list = [...direct, ...dataDeps.map((n) => data[n]), ...over].find(Array.isArray);
      return list ? list.length : undefined;
    };
    // The data in hand once the queries a change started have run.
    let loading = Promise.resolve();
    const settled = () => loading.then(() => new Promise((r) => setTimeout(r, 0)));
    // A search is recorded once typing stops, as the words someone looked for
    // and how many rows they found.
    const typing = new Map();
    const trackSearch = (key, v) => {
      clearTimeout(typing.get(key));
      typing.set(key, setTimeout(() => { const q = String(v ?? '').trim(); if (q) settled().then(() => app.track('search', q, { results: resultsFor(key) })); }, 1200));
    };
    const trackFilter = (key, value) => settled().then(() => app.track('filter', `${key}=${value}`, { results: resultsFor(key) }));
    // A form typed into and then closed unsaved (Cancel, another screen, or
    // a redraw that no longer shows it) is recorded as given up.
    const abandon = (f) => {
      if (!f || !f.touched) return;
      app.track('abandon', `${f.table} ${f.editing ? 'edit' : 'new'}`, { table: f.table, ms: Date.now() - f.touched });
      f.touched = 0;
    };
    if (window.__sa_onLeave) window.__sa_onLeave(() => { for (const f of forms.values()) if (f.el && f.el.isConnected) abandon(f); });

    const fail = (where, e) => {
      const err = new Error(`${where}: ${e && e.message ? e.message : e}`);
      err.__cap = e && e.__cap;
      // A refusal and a call nobody answered stay what they are (frame.js).
      if (e && e.refused) err.refused = true;
      if (e && e.transient) err.transient = true;
      return err;
    };
    // The value of v in scope s: expressions evaluated, objects and arrays walked.
    const val = (v, s, where = 'screen') => {
      if (isExpr(v)) {
        const f = E[v.$e];
        if (typeof f !== 'function') throw new Error(`${where}: the expression ${JSON.stringify(SRC[v.$e] || '')} does not parse`);
        try { return f(s); } catch (e) { throw fail(`${where}: ${SRC[v.$e]}`, e); }
      }
      if (Array.isArray(v)) return v.map((x, i) => val(x, s, `${where}[${i}]`));
      if (v && typeof v === 'object') { const o = {}; for (const [k, x] of Object.entries(v)) o[k] = val(x, s, `${where}.${k}`); return o; }
      return v;
    };
    const scope = (extra) => ({ ...data, ...lets, data, state, set, refresh, app, ui, lib, h, svg, fmt: app.fmt, params: app.params, today: app.today(), ...(extra || {}) });

    // A handler: run it, then fetch the data again (writes show at once).
    const run = async (fn, args, where) => {
      try {
        const r = typeof fn === 'function' ? fn(...args) : fn;
        if (r && typeof r.then === 'function') await r;
      } catch (e) {
        if (e && e.transient) app.toast('No answer from the app just now. Try again in a moment.');
        else { if (!(e && e.refused)) report(fail(where, e)); app.toast(e && e.message ? e.message : String(e)); }
      }
      await refresh();
    };
    const handler = (v, s, where) => {
      const fn = val(v, s, where);
      return typeof fn === 'function' ? (...args) => run(fn, args, where) : null;
    };
    // Callbacks inside a component's props become handlers that refresh after.
    const wrapProps = (props, where) => {
      const out = {};
      for (const [k, v] of Object.entries(props || {})) out[k] = typeof v === 'function' && /^on[A-Z]/.test(k) ? (...args) => run(v, args, `${where}.${k}`) : v;
      return out;
    };
    // A visitor who may only look (a published app's choice, app.access) is
    // not offered what would write: forms, and the buttons and row actions
    // that add, change or remove (their code writes, or their label says so:
    // Edit opens a form that is not there). The server refuses such a write
    // anyway; this keeps it from being offered.
    const readOnly = app.access === 'look';
    const WRITES = /\bapp\.(insert|update|remove|call|upload|email)\s*\(/;
    const WRITE_LABEL = /^(add|new|edit|delete|remove|archive|discard|trash|create)\b/i;
    const writes = (doV, label) => readOnly && ((isExpr(doV) && WRITES.test(String(SRC[doV.$e] || ''))) || (typeof doV === 'function' && WRITES.test(String(doV))) || WRITE_LABEL.test(String(label || '').trim()));
    // A preview (drawn while the model still writes the app) has no tables
    // or components yet: what fails stays quiet.
    const preview = !!spec.preview;
    function report(e) {
      if (preview) return;
      // Not the screen's fault: a refusal is said where it happened, and a
      // call nobody answered is asked again (load).
      if (e && (e.refused || e.transient)) return;
      const key = String(e && e.message);
      if (reported.has(key)) return;
      reported.add(key);
      window.__sa_report && window.__sa_report(e);
    }

    async function load() {
      const s = scope();
      const entries = Object.entries(spec.data || {});
      // A preview has no app to ask yet: its data is empty.
      if (preview) { data = Object.fromEntries(entries.map(([name]) => [name, []])); return; }
      const out = await Promise.all(entries.map(async ([name, q]) => {
        const where = `data.${name}`;
        try {
          if (isExpr(q)) return await val(q, s, where);
          const def = typeof q === 'string' ? { sql: q } : q || {};
          // An expression for sql gives the SELECT to run (one that depends on
          // state), or the rows themselves.
          // A query that names :today and gives it no value gets the day here.
          const bind = (sql, p) => (/:today\b/.test(sql) && (p.today === undefined || p.today === null) ? { ...p, today: app.today() } : p);
          if (isExpr(def.sql)) {
            const sql = await val(def.sql, s, where);
            if (typeof sql !== 'string') return sql;
            return await app.sql(sql, bind(sql, val(def.params || {}, s, `${where}.params`)));
          }
          return await app.sql(def.sql, bind(String(def.sql || ''), val(def.params || {}, s, `${where}.params`)));
        } catch (e) {
          // Nobody answered (the network, the engine restarting): what was
          // shown stays, and the data is asked for again shortly.
          if (e && e.transient) { missed = true; return data[name]; }
          report(fail(where, e));
          return [];
        }
      }));
      data = Object.fromEntries(entries.map(([name], i) => [name, out[i] ?? []]));
      retryLoad(missed);
    }
    // After a read nobody answered: again in 2 s, then 4, 8... up to 30 s,
    // until one is answered; the person is told once (frame.js).
    let retryIn = 0;
    let retryTimer = 0;
    let missed = false;
    function retryLoad(was) {
      missed = false;
      if (!was) { if (retryIn && window.__sa_offline) window.__sa_offline(false); retryIn = 0; return; }
      if (!retryIn && window.__sa_offline) window.__sa_offline(true);
      retryIn = Math.min(30000, retryIn ? retryIn * 2 : 2000);
      clearTimeout(retryTimer);
      retryTimer = setTimeout(() => { refresh().catch(() => {}); }, retryIn);
    }
    function compute() {
      lets = {};
      for (const [name, v] of Object.entries(spec.let || {})) {
        try { lets[name] = val(v, scope(), `let.${name}`); } catch (e) { report(e); lets[name] = undefined; }
      }
    }

    function set(changes) {
      const next = typeof changes === 'function' ? changes(state) : changes;
      state = { ...state, ...(next || {}) };
      if (hasParams) refresh(); else { compute(); draw(); }
    }
    async function refresh() { const p = load(); loading = p.catch(() => {}); await p; compute(); draw(); }
    const hasParams = Object.values(spec.data || {}).some((q) => q && typeof q === 'object' && (isExpr(q) || q.params || isExpr(q.sql)));

    // ---- blocks
    const blocks = (list, s, path) => (Array.isArray(list) ? list : list == null ? [] : [list]).map((b, i) => block(b, s, `${path}[${i}]`));
    const T = (v, s, where) => { const x = val(v, s, where); return x == null ? '' : String(x); };

    function block(b, s, path) {
      try {
        if (b == null || b === false) return null;
        if (typeof b === 'string' || isExpr(b)) return h('p', null, T(b, s, path));
        if (b.if !== undefined) return blocks(val(b.if, s, `${path}.if`) ? b.then : b.else, s, `${path}.${val(b.if, s) ? 'then' : 'else'}`);
        // Front-page blocks (web/front.js): a hero, carousels, galleries...
        if (front) { const k = front.kinds.find((x) => b[x] !== undefined); if (k) return front.blocks[k](b, s, path); }
        if (b.h !== undefined) return h(`h${b.level || 2}`, { class: b.class }, T(b.h, s, path));
        if (b.text !== undefined) return h('p', { class: [b.muted ? 'muted' : '', b.small ? 'small' : '', b.class || ''].join(' ').trim() || null }, T(b.text, s, path));
        if (b.ui !== undefined) {
          const name = val(b.ui, s, path);
          if (typeof ui[name] !== 'function') throw new Error(`no shared component ${name}`);
          const el = ui[name](wrapProps(val(b.props || {}, s, `${path}.props`), `${path}.props`));
          return el instanceof Node ? el : h('div', null, el == null ? '' : String(el));
        }
        for (const kind of ['row', 'col', 'grid', 'split']) if (b[kind] !== undefined) return h('div', { class: [kind, b.class].filter(Boolean).join(' ') }, blocks(b[kind], s, `${path}.${kind}`));
        if (b.card !== undefined) return h('div', { class: ['card', b.class].filter(Boolean).join(' ') }, b.title ? h('h3', null, T(b.title, s, path)) : null, h('div', { class: 'col' }, blocks(b.card, s, `${path}.card`)));
        if (b.section !== undefined) return h('section', { class: 'section col' }, b.title ? h('h3', null, T(b.title, s, path)) : null, blocks(b.section, s, `${path}.section`));
        if (b.each !== undefined) return each(b, s, path);
        if (b.kpis !== undefined) {
          const ks = (val(b.kpis, s, path) || []).map((k) => {
            // A KPI of a query ("of": its data) is the first value of its first row.
            let v = k.of !== undefined ? (() => { const r = (data[k.of] || [])[0]; return r ? (r.v !== undefined ? r.v : Object.values(r)[0]) : null; })() : k.value;
            if (k.of !== undefined && v == null) v = 0;
            const raw = v;
            if (k.format && FORMAT[k.format] && v != null) v = FORMAT[k.format](v);
            return { ...k, raw, shown: v == null ? '–' : String(v) };
          });
          if (lookIs() !== 'plain' && ks.length && (heroAt === null || heroAt === path)) { heroAt = path; return bold() ? hero(ks, s, path) : summary(ks); }
          return h('div', { class: 'kpis' }, ks.map((k) => h('div', { class: zero(k) ? 'kpi zero' : 'kpi', 'data-num': zero(k) ? '0' : '1' }, h('div', { class: 'value' }, k.shown), h('div', { class: 'label' }, k.label), k.hint ? h('div', { class: 'small muted' }, k.hint) : null)));
        }
        if (b.chips !== undefined) return chips(b, s, path);
        if (b.input !== undefined) return input(b, s, path);
        if (b.button !== undefined) return button(b, s, path);
        if (b.form !== undefined) return readOnly ? null : folded(b, s, path);
        if (b.checks !== undefined) return checks(b, s, path);
        if (b.table !== undefined) return table(b, s, path);
        if (b.code !== undefined) return code(b, s, path);
        throw new Error(`unknown block ${JSON.stringify(Object.keys(b))}`);
      } catch (e) {
        if (preview) return h('div', { class: 'card small muted', style: 'border-style: dashed' }, b && b.ui ? `ui.${b.ui}` : 'Being written');
        report(fail(path, e));
        return h('div', { class: 'card small', style: 'border-color: var(--bad); color: var(--bad)' }, `This part of the screen failed: ${e.message}`);
      }
    }

    function each(b, s, path) {
      const list = val(b.each, s, `${path}.each`) || [];
      if (!list.length) return h('div', { class: 'empty' }, T(b.empty ?? 'Nothing here yet.', s, path));
      const cls = b.class || 'list';
      return h('div', { class: cls }, list.map((item, i) => {
        const inner = scope({ ...s, [b.as || 'item']: item, index: i, __row: item });
        const kids = blocks(b.body, inner, `${path}.body`);
        // An item that is one card already is not boxed again.
        const one = Array.isArray(b.body) && b.body.length === 1 && b.body[0] && typeof b.body[0] === 'object' && (b.body[0].card !== undefined || b.body[0].ui !== undefined);
        if (one && kids.length === 1 && kids[0]) return kids[0];
        return cls === 'list' ? h('div', { class: 'item' }, kids) : cls === 'grid' ? h('div', { class: 'card col' }, kids) : h('div', { class: 'col' }, kids);
      }));
    }

    function chips(b, s, path) {
      const key = b.chips;
      return h('div', { class: 'chips' }, (val(b.options, s, path) || []).map(OPT).map((o) =>
        h('button', { class: `chip${state[key] === o.value ? ' on' : ''}`, type: 'button', onclick: () => { const off = state[key] === o.value && b.toggle; const changed = !off && state[key] !== o.value; set({ [key]: off ? null : o.value }); if (changed) trackFilter(key, o.value); } }, cap(o.label))));
    }

    function input(b, s, path) {
      const key = b.input;
      const type = b.type || 'text';
      const label = b.label ? T(b.label, s, path) : null;
      let el;
      if (type === 'select') {
        el = h('select', { onchange: (e) => { set({ [key]: e.target.value }); if (e.target.value) trackFilter(key, e.target.value); } }, (val(b.options, s, path) || []).map(OPT).map((o) => h('option', { value: o.value, selected: String(state[key] ?? '') === String(o.value) }, o.label)));
      } else if (type === 'checkbox') {
        el = h('input', { type: 'checkbox', checked: !!state[key], onchange: (e) => set({ [key]: e.target.checked }) });
      } else {
        el = h('input', { type: type === 'search' ? 'search' : type, placeholder: b.placeholder ? T(b.placeholder, s, path) : null, value: state[key] ?? '',
          oninput: (e) => { const v = type === 'number' ? (e.target.value === '' ? null : Number(e.target.value)) : e.target.value; set({ [key]: v }); if (type === 'search') trackSearch(key, v); } });
      }
      el.dataset.bind = `${path}`;
      return label ? h('label', { class: 'col', style: 'gap:4px' }, h('span', { class: 'small muted' }, label), el) : el;
    }

    // A question before a handler runs, except before a remove: every
    // remove offers Undo, so a delete asks nothing first.
    const asks = (q, fn) => !!q && !/\bapp\.remove\(/.test(String(fn));

    function button(b, s, path) {
      if (writes(b.do, T(b.button, s, path))) return null;
      const fn = val(b.do, s, `${path}.do`);
      const btn = h('button', { class: b.class || null, type: 'button', onclick: async () => {
        if (asks(b.confirm, fn) && !confirm(T(b.confirm, s, path))) return;
        btn.disabled = true;
        await run(fn, [], `${path}.do`);
        btn.disabled = false;
      } }, T(b.button, s, path));
      const who = rowName(s.__row);
      if (who && btn.textContent.trim().length <= 24) btn.setAttribute('aria-label', `${btn.textContent.trim() || 'Open'}: ${who}`);
      return btn;
    }

    // A form keeps its element (and what is typed) across redraws; it is made
    // again only when the row it edits changes.
    // On a phone, a form of three fields or more on a screen that lists rows
    // folds behind its button, so the rows are on the first screen; it opens
    // to edit a row, and folds again once it is saved. "fold": false keeps
    // it open (when filling it is the job), true folds it on any screen.
    const listing = /"(each|table|checks|code)"\s*:/.test(JSON.stringify(spec.body || []));
    function folded(b, s, path) {
      const el = form(b, s, path);
      const n = (b.fields || []).filter((f) => typeof f === 'string' || (f && f.name)).length;
      if (b.closes || b.fold === false || !(b.fold === true || b.fold === 'phone' || (listing && n >= 3))) return el;
      const key = `__fold:${path}`;
      const editing = b.edit !== undefined ? val(b.edit, s, `${path}.edit`) : null;
      const open = !!state[key] || !!(editing && editing.id);
      const label = b.submit !== undefined ? T(b.submit, s, path) : `Add ${human(b.form || '').replace(/s$/, '')}`;
      return h('div', { class: `fold${b.fold === true ? '' : ' phone'}${open ? ' open' : ''}` },
        h('button', { class: 'primary fold-open', type: 'button', 'aria-expanded': String(open), onclick: () => set({ [key]: true }) }, label),
        el,
        open && !(editing && editing.id) ? h('button', { class: 'ghost fold-close', type: 'button', onclick: () => set({ [key]: false }) }, 'Cancel') : null);
    }

    function form(b, s, path) {
      const inCol = b.layout === 'col';
      const tableName = b.form;
      const editing = b.edit !== undefined ? val(b.edit, s, `${path}.edit`) : null;
      const sig = `${editing && editing.id ? editing.id : ''}|${JSON.stringify(b.fields)}`;
      const kept = forms.get(path);
      if (kept && kept.sig === sig) { kept.s = s; kept.refresh(s); return kept.el; }
      if (kept) abandon(kept);
      const entry = { sig, s, el: null, table: tableName, editing: !!(editing && editing.id), touched: 0 };
      // A field may be a heading for the fields after it: {"section": "Payment"}.
      const all = (b.fields || []).map((f) => (typeof f === 'string' ? { name: f } : f));
      const fields = all.filter((f) => f && f.name);
      const controls = new Map();
      const valueOf = (f, el) => {
        const t = f.type || 'text';
        if (t === 'checkbox') return el.checked;
        if (t === 'number' || t === 'rating') return el.value === '' ? null : Number(el.value);
        if (t === 'tags') return el.value.split(',').map((x) => x.trim()).filter(Boolean);
        if (t === 'datetime') return el.value ? new Date(el.value).toISOString() : null;
        if (t === 'file') return el.__fileId || (editing ? editing[f.name] : null) || null;
        return el.value === '' ? null : el.value;
      };
      // A select's options can come from data ("=kids.map(k => k.name)"): they
      // are filled again on every draw, since the form itself is kept while
      // its rows load and change (a kid added elsewhere shows up at once).
      const fillSelect = (el, f, s, v) => {
        const opts = (val(f.options, s, path) || []).map(OPT);
        const key = JSON.stringify(opts);
        if (el.__opts === key) return;
        const had = el.__opts !== undefined ? el.value : v;
        el.__opts = key;
        el.replaceChildren(...(f.required ? [] : [h('option', { value: '' }, f.placeholder || '—')]), ...opts.map((o) => h('option', { value: o.value }, o.label)));
        if (had != null && opts.some((o) => String(o.value) === String(had))) el.value = String(had);
      };
      entry.refresh = (s2) => { for (const f of fields) if ((f.type || 'text') === 'select' && isExpr(f.options)) fillSelect(controls.get(f.name), f, s2, null); };
      // A form of more than three fields, or with a long one, is laid out
      // on a grid: every field labelled above it, long ones across the
      // whole row, checkboxes beside their label, the buttons on a row of
      // their own. A short form stays one line ([title] [due] Add).
      // A form in a column (layout "col") is a grid too: it has the room of
      // its card, and narrows to one column where the card is narrow.
      const wide = inCol ? fields.length > 1 || fields.some((f) => f.type === 'textarea' || f.type === 'file') : fields.length > 3 || fields.some((f) => f.type === 'textarea' || f.type === 'file');
      // A control a placeholder cannot name (a date, a select...) gets a
      // label; in a one-line form, then, every field does, so they line up.
      const named = (f) => !!f.label || ['checkbox', 'file', 'rating', 'date', 'time', 'datetime', 'select'].includes(f.type || 'text');
      const labelAll = !inCol && !wide && fields.length > 1 && fields.some(named);
      const make = (f) => {
        const t = f.type || 'text';
        const v = editing ? editing[f.name] : val(f.value, s, `${path}.${f.name}`);
        const ph = f.placeholder ? T(f.placeholder, s, path) : f.label || wide || labelAll ? null : human(f.name);
        let el;
        if (t === 'textarea') el = h('textarea', { name: f.name, placeholder: ph, required: !!f.required, rows: f.rows || 3 }, v ?? '');
        else if (t === 'select') { el = h('select', { name: f.name, required: !!f.required }); fillSelect(el, f, s, v); }
        else if (t === 'checkbox') el = h('input', { type: 'checkbox', name: f.name, checked: !!v });
        else if (t === 'tags') el = h('input', { name: f.name, placeholder: ph || 'tags, comma separated', value: Array.isArray(v) ? v.join(', ') : v ?? '' });
        else if (t === 'rating') el = h('select', { name: f.name }, h('option', { value: '' }, '—'), [1, 2, 3, 4, 5].map((n) => h('option', { value: n, selected: Number(v) === n }, '★'.repeat(n))));
        else if (t === 'file') {
          el = h('input', { type: 'file', name: f.name, accept: f.accept || null });
          el.addEventListener('change', async () => { if (el.files[0]) { const up = await app.upload(el.files[0]); el.__fileId = up.id; } });
        } else {
          const typ = { date: 'date', time: 'time', datetime: 'datetime-local', number: 'number', email: 'email', url: 'url', color: 'color' }[t] || 'text';
          let shown = v ?? '';
          if (t === 'datetime' && v) { const d = new Date(v); shown = new Date(d.getTime() - d.getTimezoneOffset() * 60000).toISOString().slice(0, 16); }
          el = h('input', { type: typ, name: f.name, placeholder: ph, required: !!f.required, value: shown, step: t === 'number' ? (f.step || 'any') : null, min: f.min ?? null, max: f.max ?? null });
        }
        controls.set(f.name, el);
        if (wide) {
          const text = f.label ? T(f.label, s, path) : human(f.name);
          const hint = f.hint ? h('span', { class: 'hint' }, T(f.hint, s, path)) : null;
          if (t === 'checkbox') return h('label', { class: 'check' }, el, h('span', null, text), hint);
          return h('label', { class: t === 'textarea' || t === 'file' || t === 'tags' ? 'span' : null }, h('span', null, text, f.required ? h('span', { class: 'req', 'aria-hidden': 'true' }, ' *') : null), el, hint);
        }
        // In a column a control keeps its own height (a form's controls flex
        // along the row).
        if (inCol && t !== 'checkbox') el.style.flex = 'none';
        if (inCol && t !== 'checkbox') el.style.width = '100%';
        const lab = f.label ? T(f.label, s, path) : labelAll || named(f) ? human(f.name) : null;
        return lab ? h('label', { class: 'col', style: inCol ? 'gap:4px' : 'gap:4px;flex:1 1 140px' }, h('span', { class: 'small muted' }, lab), el) : el;
      };
      const saveBtn = h('button', { class: 'primary', type: 'submit', style: inCol && !wide ? 'align-self:flex-start' : null }, T(editing ? (b.update || 'Save') : (b.submit || 'Add'), s, path));
      const el = h('form', { class: wide ? 'wide' : inCol ? 'col' : null, style: inCol && !wide ? 'align-items:stretch' : null, onsubmit: async (e) => {
        e.preventDefault();
        const cur = entry.s;
        const row = {};
        for (const f of fields) row[f.name] = valueOf(f, controls.get(f.name));
        Object.assign(row, val(b.set || {}, cur, `${path}.set`));
        for (const f of fields) if (f.required && (row[f.name] === null || row[f.name] === '')) { app.toast(`${f.label || human(f.name)} is needed`); return; }
        saveBtn.disabled = true;
        try {
          const stored = editing && editing.id ? await app.update(tableName, editing.id, row) : await app.insert(tableName, row);
          entry.touched = 0;
          if (!editing) { el.reset(); for (const c of controls.values()) c.__fileId = null; }
          forms.delete(path);
          if (state[`__fold:${path}`]) set({ [`__fold:${path}`]: false });
          const after = b.after !== undefined ? val(b.after, cur, `${path}.after`) : null;
          await run(typeof after === 'function' ? after : null, [stored], `${path}.after`);
        } catch (err) { app.toast(err.message); report(fail(`${path} (${tableName})`, err)); } finally { saveBtn.disabled = false; }
      } }, wide ? [] : fields.map(make));
      if (wide) {
        // Each section (the fields after a {"section"} heading, or all of
        // them): short fields first, as many columns as make even rows (5
        // fields: 3 and 2, not 4 and 1), then the long ones across, then the
        // checkboxes together; each group keeps the order it was given in.
        const long = (f) => ['textarea', 'file', 'tags'].includes(f.type);
        const groups = [];
        for (const f of all) {
          if (f && f.section !== undefined && !f.name) groups.push({ title: T(f.section, s, path), hint: f.hint ? T(f.hint, s, path) : null, fields: [] });
          else if (f && f.name) { if (!groups.length) groups.push({ title: null, fields: [] }); groups.at(-1).fields.push(f); }
        }
        const most = Math.max(1, ...groups.map((g) => g.fields.filter((f) => !long(f) && f.type !== 'checkbox').length));
        const rows = Math.max(1, Math.ceil(most / 4));
        el.style.setProperty('--cols', String(Math.max(1, Math.ceil(most / rows))));
        for (const g of groups) {
          if (g.title) el.append(h('div', { class: 'part' }, h('span', null, g.title), g.hint ? h('span', { class: 'hint' }, g.hint) : null));
          const short = g.fields.filter((f) => !long(f) && f.type !== 'checkbox');
          el.append(...short.map(make), ...g.fields.filter(long).map(make));
          const checks = g.fields.filter((f) => f.type === 'checkbox');
          if (checks.length) el.append(h('div', { class: 'checks' }, checks.map(make)));
        }
      }
      // Cancel shows while a row is edited, or always on a form that opens
      // and closes ("closes": true, as a pattern's New form does).
      const cancelBtn = (editing || b.closes) && b.cancel !== undefined ? h('button', { class: 'ghost', type: 'button', onclick: () => { abandon(entry); forms.delete(path); const c = val(b.cancel, entry.s, `${path}.cancel`); run(c, [], `${path}.cancel`); } }, 'Cancel') : null;
      if (wide) el.append(h('div', { class: 'actions' }, cancelBtn, saveBtn));
      else el.append(...[saveBtn, cancelBtn].filter(Boolean));
      el.addEventListener('input', () => { if (!entry.touched) entry.touched = Date.now(); });
      entry.el = el;
      forms.set(path, entry);
      return el;
    }

    // How a value reads: a column's format word, a KPI's. Dates are
    // 'YYYY-MM-DD' days or ISO times; durations are minutes, clocks seconds.
    const DAY_MS = 86400000;
    const dayOf = (v) => (v == null || v === '' ? null : Date.parse(`${String(v).slice(0, 10)}T00:00:00Z`));
    const num = (v) => (v == null || v === '' || !Number.isFinite(Number(v)) ? null : Number(v));
    const two = (n) => String(n).padStart(2, '0');
    const FORMAT = {
      date: (v) => app.fmt.date(v), datetime: (v) => app.fmt.datetime(v), time: (v) => app.fmt.time(v), ago: (v) => app.fmt.ago(v),
      money: (v) => app.fmt.money(v), number: (v) => app.fmt.number(num(v) == null ? v : Math.round(num(v) * 100) / 100),
      percent: (v) => (num(v) == null ? '' : `${Math.round(num(v) * 100)}%`), pct: (v) => (num(v) == null ? '' : `${Math.round(num(v) * 10) / 10}%`),
      hours: (v) => (num(v) == null ? '' : `${Math.round(num(v) * 10) / 10} h`),
      duration: (v) => { const m = num(v); if (m == null) return ''; const t = Math.round(m); return t < 60 ? `${t} min` : `${Math.floor(t / 60)} h ${two(t % 60)}`; },
      clock: (v) => { const x = num(v); if (x == null) return ''; const t = Math.round(x); const h = Math.floor(t / 3600); return h ? `${h}:${two(Math.floor(t / 60) % 60)}:${two(t % 60)}` : `${Math.floor(t / 60)}:${two(t % 60)}`; },
      days: (v) => {
        const d = dayOf(v);
        if (d == null || !Number.isFinite(d)) return v == null ? '' : String(v);
        const n = Math.round((d - dayOf(app.today())) / DAY_MS);
        return n === 0 ? 'today' : n === 1 ? 'tomorrow' : n === -1 ? 'yesterday' : n > 0 ? `in ${n} days` : `${-n} days ago`;
      },
      weekday: (v) => { const d = dayOf(v); return d == null || !Number.isFinite(d) ? '' : new Date(d).toLocaleDateString(undefined, { weekday: 'long', timeZone: 'UTC' }); },
      month: (v) => { const d = dayOf(String(v || '').length === 7 ? `${v}-01` : v); return d == null || !Number.isFinite(d) ? '' : new Date(d).toLocaleDateString(undefined, { month: 'long', year: 'numeric', timeZone: 'UTC' }); },
      stars: (v) => (v ? '★'.repeat(Math.round(v)) : ''), bool: (v) => (v ? '✓' : ''),
    };
    // A badge's color from what its word means (paid, late, waiting, new...);
    // a column may name them itself: "badge": {"shipped": "ok", "lost": "bad"}.
    const TONES = {
      ok: /^(paid|done|complete[d]?|yes|active|ok|okay|approved|accepted|delivered|ready|confirmed|present|won|live|published|available|resolved|success|passed|good|in stock|healthy|on track|finished|kept|enabled|true|✓|returned|received|fulfilled|signed)$/,
      bad: /^(late|overdue|no|failed|fail|cancell?ed|rejected|out|out of stock|lost|absent|declined|blocked|error|urgent|expired|unpaid|critical|missing|broken|disabled|false|off track|bounced|damaged|needs repair|lost|refunded)$/,
      warn: /^(due|waiting|pending|low|low stock|maybe|partial|partly paid|in progress|doing|preparing|review|in review|tentative|soon|on hold|hold|reorder|draft|stale|warning|busy|sent|fair|worn|out now|rented|on loan)$/,
      info: /^(new|planned|scheduled|todo|to do|open|booked|invited|queued|next|info)$/,
    };
    function toneOf(v, given) {
      const w = String(v).trim().toLowerCase();
      if (given && typeof given === 'object' && given[w] !== undefined) return given[w] || 'plain';
      for (const [tone, re] of Object.entries(TONES)) if (re.test(w)) return tone;
      return 'plain';
    }
    function table(b, s, path) {
      const rows = val(b.table, s, `${path}.table`) || [];
      if (!rows.length) return h('div', { class: 'empty' }, T(b.empty ?? 'Nothing here yet.', s, path));
      const given = b.columns !== undefined ? val(b.columns, s, `${path}.columns`) : null;
      const cols = (Array.isArray(given) && given.length ? given : Object.keys(rows[0]).filter((k) => k !== 'id' && k !== 'created').map((key) => ({ key }))).map((c) => (typeof c === 'string' ? { key: c } : c));
      const fns = cols.map((c, i) => (c.value !== undefined ? val(c.value, s, `${path}.columns[${i}].value`) : null));
      const acts = (b.actions || []).map((a, i) => ({ ...a, fn: val(a.do, s, `${path}.actions[${i}].do`) })).filter((a) => !writes(a.do, T(a.label, s, path)));
      const onRow = b.onRow !== undefined ? val(b.onRow, s, `${path}.onRow`) : null;
      const cell = (c, fn, r) => {
        let v = typeof fn === 'function' ? fn(r) : fn !== null ? fn : r[c.key];
        // A tick in a row writes when it changes: one a visitor who may only look cannot change.
        if (readOnly && v instanceof HTMLInputElement && v.type === 'checkbox') v.disabled = true;
        if (v instanceof Node) return v;
        if (c.format && FORMAT[c.format]) v = FORMAT[c.format](v);
        if (Array.isArray(v)) v = v.join(', ');
        return c.badge && v != null && v !== '' ? h('span', { class: `badge ${toneOf(v, c.badge)}` }, cap(String(v))) : v == null ? '' : String(v);
      };
      // More than three columns is a list of cards on a phone (kit.css).
      const heads = cols.map((c) => c.label ?? human(c.key || ''));
      if (phone.matches && lookIs() !== 'plain') return (bold() ? rowCards : rowCells)(rows, cols, fns, acts, onRow, heads, cell, s, path);
      return h('div', { style: 'overflow-x:auto' }, h('table', { class: cols.length > 3 ? 'stack' : null },
        h('thead', null, h('tr', null, heads.map((x) => h('th', null, x)), acts.length ? h('th') : null)),
        h('tbody', null, rows.map((r) => h('tr', { style: onRow ? 'cursor:pointer' : null, onclick: onRow ? (e) => { if (!e.target.closest('button')) run(onRow, [r], `${path}.onRow`); } : null },
          cols.map((c, i) => {
            const v = cell(c, fns[i], r);
            // A tick in a row is a 44 px target named by its row (kit.css).
            if (v instanceof HTMLInputElement && v.type === 'checkbox') {
              if (!v.getAttribute('aria-label')) v.setAttribute('aria-label', `${heads[i] || 'Done'}: ${rowName(r) || ''}`.replace(/: $/, ''));
              return h('td', { class: 'tick', 'data-label': heads[i] }, h('label', { class: 'tap' }, v));
            }
            return h('td', { 'data-label': heads[i], class: v === '' ? 'blank' : null }, v);
          }),
          // More than two steps: the row's next step as a button, the rest
          // in its menu (as on a phone), not a line of words on every row.
          acts.length > 2 ? h('td', { class: 'right acts' }, (() => { const { main, more } = rowActions(acts, r, rowName(r), s, path, (l) => `rc-pill${SOFT.test(l) ? ' soft' : ''}`); return h('span', { class: 'rc-end' }, main, more); })())
          : acts.length ? h('td', { class: 'nowrap right' }, acts.map((a) => {
            const label = T(a.label, s, path);
            const who = rowName(r);
            return h('button', { class: a.class || 'small ghost', type: 'button', 'aria-label': who ? `${label}: ${who}` : null, onclick: () => { if (asks(a.confirm, a.fn) && !confirm(T(a.confirm, scope({ ...s, row: r }), path))) return; run(a.fn, [r], `${path}.actions`); } }, label);
          })) : null)))));
    }

    // ---- the bold look
    // The first big numbers of a screen as a band: the one that needs doing
    // (overdue, low, open... and not zero) large, the others beside it small.
    const URGENT = /\b(overdue|late|owed|owing|unpaid|out|missing|urgent|expired|failed|behind)\b/i;
    const ATTN = /\b(overdue|late|owed|owing|unpaid|due|out|low|reorder|to order|open|to do|todo|pending|waiting|new|missing|urgent|left|behind|needs?|unanswered|unread|expiring|expired|failed)\b/i;
    const zero = (k) => k.raw == null || k.raw === '' || Number(k.raw) === 0 || /^[^0-9]*0([.,]0+)?[^0-9]*$/.test(k.shown);
    const lower = (t) => (/^[A-Z][a-z]/.test(t) ? t[0].toLowerCase() + t.slice(1) : t);
    const mainOf = (ks) => ks.find((k) => URGENT.test(String(k.label || '')) && !zero(k)) || ks.find((k) => ATTN.test(String(k.label || '')) && !zero(k)) || ks.find((k) => !zero(k)) || ks[0];
    // Native: the numbers as one sentence under the title, the one that needs
    // doing first and strong ("1 out · 2 low · 5 items").
    function summary(ks) {
      const main = mainOf(ks);
      const rest = ks.filter((k) => k !== main);
      const said = (k) => `${k.shown}${k.label ? ` ${lower(String(k.label))}` : ''}`;
      return h('div', { class: 'say' },
        h('p', null, h('b', { class: URGENT.test(String(main.label || '')) && !zero(main) ? 'attn' : null, 'data-num': zero(main) ? '0' : '1' }, said(main)), rest.map((k) => h('span', { class: zero(k) ? 'zero' : null, 'data-num': zero(k) ? '0' : '1' }, ` · ${said(k)}`))),
        main.hint ? h('p', { class: 'muted' }, main.hint) : null);
    }
    function hero(ks, s, path) {
      const main = mainOf(ks);
      const rest = ks.filter((k) => k !== main);
      const tone = ATTN.test(String(main.label || '')) && !zero(main) ? ' attn' : '';
      return h('section', { class: `hero-band${tone}`, 'aria-label': `${main.shown} ${main.label || ''}`.trim() },
        h('div', { class: 'hero-big', 'data-num': zero(main) ? '0' : '1' }, h('b', null, main.shown), main.label ? h('span', null, lower(String(main.label))) : null),
        main.hint ? h('p', { class: 'hero-line' }, main.hint) : null,
        rest.length ? h('div', { class: 'hero-legend' }, rest.map((k) => h('span', { class: zero(k) ? 'zero' : null, 'data-num': zero(k) ? '0' : '1' }, h('b', null, k.shown), ' ', lower(String(k.label || ''))))) : null);
    }

    // A row as a card: its name and its other words, the number that matters
    // large (with a level bar when another column is its level: stock and
    // its reorder point, spent and its budget), its state as a pill, and its
    // next step as a button, the other actions behind "More".
    const HALF = /(reorder|minimum|\bmin\b|\bpar\b|threshold|safety|alert)/i;
    const FULL = /(target|goal|budget|capacity|limit|\bmax|quota|total|planned|of\b)/i;
    const DESTROY = /^(delete|remove|archive|discard|trash|cancel)\b/i;
    const SOFT = /^(edit|open|view|details?|change|more)\b/i;
    const NOT_FIGURE = /(^id$|_id$|year|phone|zip|postcode|sku|code|number$|^no$|#)/i;
    const isNum = (v) => (typeof v === 'number' && Number.isFinite(v)) || (typeof v === 'string' && /^-?\d+(\.\d+)?$/.test(v.trim()) && !/^0\d/.test(v.trim()));
    // What each column of a table is, from all its rows: the tick, the
    // state (its badge column), the name, the number that matters and the
    // level it is measured against.
    function roles(rows, cols, fns, heads) {
      const raw = (c, i, r) => { const fn = fns[i]; const v = typeof fn === 'function' ? fn(r) : fn !== null ? fn : r[c.key]; return v; };
      const name = (c, i) => `${c.key || ''} ${heads[i] || ''}`;
      const tick = cols.findIndex((c, i) => raw(c, i, rows[0]) instanceof HTMLInputElement);
      const badge = cols.findIndex((c) => c.badge);
      const numeric = cols.map((c, i) => {
        if (i === tick || i === badge || NOT_FIGURE.test(name(c, i))) return false;
        if (['money', 'number', 'percent', 'pct', 'hours', 'duration'].includes(c.format)) return true;
        const vs = rows.map((r) => raw(c, i, r)).filter((v) => v != null && v !== '');
        return vs.length > 0 && vs.filter(isNum).length >= vs.length * 0.8;
      });
      const title = cols.findIndex((c, i) => i !== tick && i !== badge && !numeric[i]);
      const ref = cols.findIndex((c, i) => numeric[i] && (HALF.test(name(c, i)) || FULL.test(name(c, i))));
      const fig = cols.findIndex((c, i) => numeric[i] && i !== ref);
      const half = ref >= 0 && HALF.test(name(cols[ref], ref));
      return { raw, tick, badge, numeric, title, ref, fig, half };
    }
    // One row's parts as text: its state and tone, and its other words,
    // short: names as they are, numbers and dates with their column's name
    // ("Reorder at 200").
    function rowParts(R, r, cols, fns, heads, cell) {
      const text = (i) => { const v = cell(cols[i], fns[i], r); return v instanceof Node ? v.textContent : String(v ?? ''); };
      const state = R.badge >= 0 ? cell(cols[R.badge], fns[R.badge], r) : null;
      const tone = state instanceof Node ? (state.className.match(/\b(ok|warn|bad|info)\b/) || [])[1] || '' : '';
      const sub = cols.map((c, i) => {
        if (i === R.tick || i === R.badge || i === R.title || i === R.fig) return null;
        const t = text(i).trim();
        if (!t) return null;
        return R.numeric[i] || c.format ? `${heads[i]} ${t}` : t;
      }).filter(Boolean).slice(0, 4);
      return { text, state, tone, sub };
    }
    // A row's actions: the next step (the first action that does not
    // remove) as a button, the others in a menu behind "More".
    // editByTap: when the row's first step is only Edit, a tap on the row
    // edits it (as in a phone's lists) and Edit waits in the menu, rather
    // than an Edit button on every row.
    // A row's menu opens over the page, not inside its list or table (a
    // table scrolls sideways, and would cut it off): placed by the button,
    // below it when there is room, else above; one menu open at a time.
    function placeMenu(e) {
      const d = e.currentTarget;
      const menu = d.querySelector('.rc-menu');
      if (!menu) return;
      if (!d.open) { menu.removeAttribute('style'); return; }
      for (const o of document.querySelectorAll('details.rc-more[open]')) if (o !== d) o.open = false;
      const r = d.querySelector('summary').getBoundingClientRect();
      const tall = menu.offsetHeight || 200;
      const below = r.bottom + 6 + tall < window.innerHeight || r.top < tall + 6;
      Object.assign(menu.style, { position: 'fixed', right: `${Math.max(8, window.innerWidth - r.right)}px`, left: 'auto', top: below ? `${r.bottom + 6}px` : 'auto', bottom: below ? 'auto' : `${window.innerHeight - r.top + 6}px` });
      if (!placeMenu.closing) {
        placeMenu.closing = true;
        const close = (ev) => { if (!ev.target.closest?.('details.rc-more')) for (const o of document.querySelectorAll('details.rc-more[open]')) o.open = false; };
        document.addEventListener('click', close);
        window.addEventListener('scroll', () => { for (const o of document.querySelectorAll('details.rc-more[open]')) o.open = false; }, { passive: true, capture: true });
      }
    }
    function rowActions(acts, r, who, s, path, mainClass, editByTap = false) {
      const label = (a) => T(a.label, s, path);
      const doIt = (a) => () => { if (asks(a.confirm, a.fn) && !confirm(T(a.confirm, scope({ ...s, row: r }), path))) return; run(a.fn, [r], `${path}.actions`); };
      const first = acts.find((a) => /\bprimary\b/.test(a.class || '')) || acts.find((a) => !DESTROY.test(label(a)));
      const edit = editByTap && first && /^edit\b/i.test(String(label(first)).trim()) ? first : null;
      const others = acts.filter((a) => a !== first || edit);
      const main = first && !edit ? h('button', { class: mainClass(label(first)), type: 'button', 'aria-label': who ? `${label(first)}: ${who}` : null, onclick: doIt(first) }, label(first)) : null;
      const more = others.length ? h('details', { class: 'rc-more', ontoggle: placeMenu },
        h('summary', { 'aria-label': `More${who ? `: ${who}` : ''}`, title: 'More' }, h('span', { 'aria-hidden': 'true' }, '•••')),
        h('div', { class: 'rc-menu' }, others.map((a) => h('button', { class: `ghost${DESTROY.test(label(a)) ? ' danger' : ''}`, type: 'button', 'aria-label': who ? `${label(a)}: ${who}` : null, onclick: doIt(a) }, label(a))))) : null;
      return { main, more, tap: edit ? doIt(edit) : null };
    }
    const tickOf = (R, r, cols, fns, heads, cell, who) => {
      const el = R.tick >= 0 ? cell(cols[R.tick], fns[R.tick], r) : null;
      if (el instanceof HTMLInputElement && !el.getAttribute('aria-label')) el.setAttribute('aria-label', `${heads[R.tick] || 'Done'}: ${who || ''}`.replace(/: $/, ''));
      return el instanceof HTMLInputElement ? el : null;
    };

    // Native, on a phone: a table's rows as cells of grouped lists, a group
    // per state (the worst first: out, then low, then the rest), each cell
    // an icon in the state's color (or the row's initials), its name, one
    // line of detail led by its number when its state needs doing, and on
    // the right its next step, or its number.
    const TONE_ORDER = ['bad', 'warn', 'info', '', 'ok'];
    const NEEDS_DOING = /\b(out|low|overdue|late|due|unpaid|owed|owing|missing|expired|expiring|failed|reorder|to order|urgent|behind|blocked)\b/i;
    const GLYPH = { bad: '✕', warn: '!', ok: '✓', info: '•' };
    function rowCells(rows, cols, fns, acts, onRow, heads, cell, s, path) {
      const R = roles(rows, cols, fns, heads);
      const items = rows.map((r) => ({ r, ...rowParts(R, r, cols, fns, heads, cell) }));
      const groups = [];
      for (const it of items) {
        // a state left blank ("-", "—") is no group of its own
        const key = R.badge >= 0 ? it.text(R.badge).trim().replace(/^[-–—]+$/, '') : '';
        let g = groups.find((x) => x.key === key);
        if (!g) groups.push((g = { key, tone: it.tone, items: [] }));
        g.items.push(it);
      }
      groups.sort((a, b) => TONE_ORDER.indexOf(a.tone) - TONE_ORDER.indexOf(b.tone));
      // Grouped by state only when a state needs doing (out, low, overdue...);
      // otherwise one list in the query's order (newest runs, most votes), the
      // state said on each row.
      const stated = R.badge >= 0 && groups.length > 1 && !groups.some((g) => NEEDS_DOING.test(g.key));
      if (stated) {
        groups.splice(0, groups.length, { key: '', tone: '', items });
        for (const it of items) { const st = it.text(R.badge).trim().replace(/^[-–—]+$/, ''); if (st && !it.sub.some((x) => x.toLowerCase() === st.toLowerCase())) it.sub = [cap(st), ...it.sub].slice(0, 4); }
      }
      const initials = (t) => t.split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0]).join('').toUpperCase();
      // a row named by its day ("Nov 8, 2026", "2026-11-08") gets a calendar
      // leaf, the month over the day, rather than initials like "N8"
      const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
      const dayOf = (t) => {
        let m = /^(\d{4})-(\d{2})-(\d{2})\b/.exec(t);
        if (m && +m[2] >= 1 && +m[2] <= 12) return { mon: MONTHS[+m[2] - 1], day: +m[3] };
        m = /^(?:[A-Z][a-z]{2},? )?([A-Z][a-z]{2})[a-z]*\.? (\d{1,2})\b/.exec(t);
        return m && MONTHS.includes(m[1]) ? { mon: m[1], day: +m[2] } : null;
      };
      return h('div', { class: 'rgroups' }, groups.map((g) => h('section', { class: 'rgroup' },
        g.key ? h('h3', { class: 'rgroup-t' }, cap(g.key)) : null,
        h('div', { class: 'rgroup-list' }, g.items.map(({ r, text, tone, sub }) => {
          const who = rowName(r);
          const name = (R.title >= 0 ? text(R.title) : '') || who || 'Untitled';
          const figText = R.fig >= 0 ? text(R.fig).trim() : '';
          const needs = tone === 'bad' || tone === 'warn';
          // "None left", "4 left" for a count of things; "$650 owed" for the rest.
          const figName = String(heads[R.fig] || '').toLowerCase();
          const counted = /(stock|qty|quantity|on hand|left|remaining|units|count|in store)/.test(figName);
          const leadText = !figText ? '' : counted ? (Number(R.raw(cols[R.fig], R.fig, r)) === 0 ? 'None left' : `${figText} left`) : `${figText} ${figName}`.trim();
          const lead = needs && leadText ? h('span', { class: 'lead' }, leadText) : null;
          const { main, more, tap } = rowActions(acts, r, who, s, path, (l) => `rc-pill${SOFT.test(l) ? ' soft' : ''}`, !onRow);
          const open = onRow ? () => run(onRow, [r], `${path}.onRow`) : tap;
          const tickEl = tickOf(R, r, cols, fns, heads, cell, who);
          const day = needs ? null : dayOf(name);
          const icon = tickEl ? h('label', { class: 'tap rc-tick' }, tickEl)
            : day ? h('span', { class: 'rc-icon rc-day', 'aria-hidden': 'true' }, h('small', null, day.mon), h('b', null, String(day.day)))
            : h('span', { class: `rc-icon${tone ? ` t-${tone}` : ''}`, 'aria-hidden': 'true' }, GLYPH[tone] || initials(name) || '•');
          const end = main || (figText && !lead ? h('span', { class: 'rc-val' }, figText) : null);
          return h('div', { class: `rcell${tone ? ` t-${tone}` : ''}${tickEl && tickEl.checked ? ' done' : ''}${open ? ' tappable' : ''}`,
            onclick: open ? (e) => { if (!e.target.closest('button, summary, input, label, a')) open(); } : null },
            icon,
            h('div', { class: 'rc-name' }, h('div', { class: 'rc-title' }, name), lead || sub.length ? h('div', { class: 'rc-sub' }, lead, lead && sub.length ? ' · ' : null, sub.join(' · ')) : null),
            end || more || open ? h('div', { class: 'rc-end' }, end, more, open && !main ? h('span', { class: 'chev', 'aria-hidden': 'true' }) : null) : null);
        })))));
    }

    function rowCards(rows, cols, fns, acts, onRow, heads, cell, s, path) {
      const R = roles(rows, cols, fns, heads);
      const { raw, title, ref, fig, half } = R;
      return h('div', { class: 'rcards' }, rows.map((r) => {
        const who = rowName(r);
        const { text, state, tone, sub } = rowParts(R, r, cols, fns, heads, cell);
        let level = null;
        if (fig >= 0 && ref >= 0) {
          const a = Number(raw(cols[fig], fig, r)); const b = Number(raw(cols[ref], ref, r));
          if (Number.isFinite(a) && Number.isFinite(b) && b > 0) {
            const pct = Math.max(0, Math.min(1, half ? a / (2 * b) : a / b)) * 100;
            level = h('div', { class: 'rc-level', role: 'img', 'aria-label': `${heads[fig]} ${text(fig)}, ${heads[ref]} ${text(ref)}` },
              h('i', { style: `width:${Math.max(pct, 1.5)}%` }), half ? h('u', { style: 'left:50%' }) : null);
          }
        }
        const { main, more } = rowActions(acts, r, who, s, path, (l) => (SOFT.test(l) ? 'rc-soft' : 'primary'));
        const tickEl = tickOf(R, r, cols, fns, heads, cell, who);
        return h('article', { class: `rcard${tone ? ` t-${tone}` : ''}${tickEl && tickEl.checked ? ' done' : ''}`, style: onRow ? 'cursor:pointer' : null,
          onclick: onRow ? (e) => { if (!e.target.closest('button, summary, input, label, a')) run(onRow, [r], `${path}.onRow`); } : null },
          h('div', { class: 'rc-head' },
            tickEl ? h('label', { class: 'tap rc-tick' }, tickEl) : null,
            h('div', { class: 'rc-name' }, h('div', { class: 'rc-title' }, title >= 0 ? text(title) || who || 'Untitled' : who || 'Untitled'), sub.length ? h('div', { class: 'rc-sub' }, sub.join(' · ')) : null),
            fig >= 0 && text(fig) ? h('div', { class: 'rc-fig' }, h('b', null, text(fig)), h('small', null, String(heads[fig] || '').toLowerCase())) : null),
          level,
          state || main || more ? h('div', { class: 'rc-foot' }, state || h('span'), h('div', { class: 'rc-acts' }, main, more)) : null);
      }));
    }

    // A check per item per day (habits, chores, medication, plants watered):
    // one big tap for today on each item, the last days beside it (fewer on a
    // phone), the streak by the name, and how many are done today. A tap adds
    // or removes the row of that item and day in the log table.
    //   {"checks": "=habits", "done": "=checkins", "table": "checkins",
    //    "item": "habit", "day": "day", "name": "name", "days": 7}
    function checks(b, s, path) {
      const items = val(b.checks, s, `${path}.checks`) || [];
      const log = val(b.done, s, `${path}.done`) || [];
      const table = T(b.table, s, path);
      if (!table) throw new Error('checks needs "table": the table a check is a row of');
      const itemCol = b.item || 'item';
      const dayCol = b.day || 'day';
      const skipCol = typeof b.skip === 'string' && b.skip ? b.skip : null;
      const n = Math.max(1, Math.min(14, Number(val(b.days, s, path)) || 7));
      if (!items.length) return h('div', { class: 'empty' }, T(b.empty ?? 'Nothing to check off yet.', s, path));
      const pad = (x) => String(x).padStart(2, '0');
      const ymd = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
      const back = (k) => { const d = new Date(); d.setHours(12, 0, 0, 0); d.setDate(d.getDate() - k); return d; };
      const days = Array.from({ length: n }, (_, k) => back(n - 1 - k));
      const today = ymd(days[n - 1]);
      const has = new Map();
      for (const r of log) if (r && r[itemCol] != null && r[dayCol] != null) has.set(`${r[itemCol]}|${String(r[dayCol]).slice(0, 10)}`, r);
      const key = (it, d) => `${it.id}|${d}`;
      const nameOf = (it) => (b.name ? String(it[b.name] ?? '') : rowName(it)) || 'Untitled';
      // A day is done, skipped (with SKIP: it keeps the streak without adding
      // to it) or not done.
      const stateOf = (it, d) => { const r = has.get(key(it, d)); return !r ? null : skipCol && r[skipCol] ? 'skip' : 'done'; };
      const streak = (it) => {
        let k = stateOf(it, today) ? 0 : 1;
        let c = 0;
        for (let st = stateOf(it, ymd(back(k))); st && k < 400; st = stateOf(it, ymd(back(++k)))) if (st === 'done') c++;
        return c;
      };
      const label = (d) => (ymd(d) === today ? 'today' : d.toLocaleDateString(undefined, { weekday: 'long', month: 'short', day: 'numeric' }));
      // A tap on an empty day marks it done; on a done or skipped day, clears it.
      const toggle = async (it, d, btn, skip = false) => {
        const day = ymd(d);
        const row = has.get(key(it, day));
        btn.setAttribute('aria-pressed', String(!row));
        btn.classList.toggle('on', !row && !skip);
        btn.disabled = true;
        try {
          if (row) { await app.remove(table, row.id, { undo: false }); has.delete(key(it, day)); }
          else { has.set(key(it, day), await app.insert(table, { ...(b.fields || {}), [itemCol]: it.id, [dayCol]: day, ...(skipCol ? { [skipCol]: skip } : {}) })); }
        } catch (e) { if (e && (e.refused || e.transient)) app.toast(e.transient ? 'No answer from the app just now. Try again in a moment.' : e.message); report(fail(path, e)); }
        btn.disabled = false;
        await refresh();
      };
      const dayBtn = (it, d, k) => {
        const st = stateOf(it, ymd(d));
        const now = k === n - 1;
        const btn = h('button', { type: 'button', class: `check${now ? ' today' : ' past'}${st === 'done' ? ' on' : st === 'skip' ? ' skip' : ''}`, 'data-age': String(n - 1 - k), 'aria-pressed': String(st === 'done'),
          'aria-label': `${nameOf(it)}, ${label(d)}${st === 'skip' ? ', skipped' : ''}`, title: `${label(d)}${st === 'skip' ? ' (skipped)' : ''}`, disabled: readOnly, onclick: () => toggle(it, d, btn) },
          h('span', { class: st === 'skip' ? 'dash' : 'mark', 'aria-hidden': 'true' }));
        return btn;
      };
      // The days named once, above the rows, in the same columns.
      const legend = h('div', { class: 'checks-legend', 'aria-hidden': 'true' }, h('div', { class: 'checks-name' }),
        h('div', { class: 'checks-days' }, days.map((d, k) => h('span', { class: `check-day${k === n - 1 ? ' today' : ' past'}`, 'data-age': String(n - 1 - k) },
          k === n - 1 ? 'Today' : d.toLocaleDateString(undefined, { weekday: 'short' })))));
      const doneToday = items.filter((it) => stateOf(it, today) === 'done').length;
      const skipped = items.filter((it) => stateOf(it, today) === 'skip').length;
      const due = items.length - skipped;
      const all = doneToday === due;
      return h('div', { class: `checks${all ? ' all' : ''}` },
        b.summary === false ? null : h('div', { class: 'checks-head' },
          h('div', { class: 'checks-count' }, h('b', `${doneToday} of ${due}`), all ? ' done today. Well done.' : ' done today', skipped ? h('span', { class: 'muted' }, ` · ${skipped} skipped`) : null),
          h('div', { class: 'checks-bar', role: 'progressbar', 'aria-valuemin': '0', 'aria-valuemax': String(due), 'aria-valuenow': String(doneToday) }, h('span', { style: `width:${due ? Math.round((doneToday / due) * 100) : 100}%` }))),
        legend,
        h('ul', { class: 'checks-list' }, items.map((it) => {
          const st = b.streak === false ? 0 : streak(it);
          const todayIs = stateOf(it, today);
          return h('li', { class: `checks-row${todayIs ? ` ${todayIs === 'skip' ? 'skipped' : 'done'}` : ''}` },
            h('div', { class: 'checks-name' }, h('span', { class: 'title' }, nameOf(it)),
              b.streak === false ? null : h('span', { class: `streak${st ? '' : ' none'}` }, st ? `${st}-day streak` : 'No streak yet'),
              skipCol && !todayIs ? h('button', { type: 'button', class: 'link skip-today', 'aria-label': `Skip ${nameOf(it)} today`, onclick: (e) => toggle(it, days[n - 1], e.currentTarget, true) }, 'Skip today') : null,
              skipCol && todayIs === 'skip' ? h('span', { class: 'streak none' }, 'Skipped today') : null),
            h('div', { class: 'checks-days' }, days.map((d, k) => dayBtn(it, d, k))));
        })));
    }

    function code(b, s, path) {
      const make = CODE[b.code && b.code.$c];
      if (typeof make !== 'function') throw new Error('the code block does not parse');
      const render = make();
      if (typeof render !== 'function') throw new Error('the code block defines no render(root, ctx)');
      const box = h('div');
      const ctx = { state, set, refresh, data: { ...data, ...lets }, app, ...data, ...lets };
      Promise.resolve().then(() => render(box, ctx)).catch((e) => { report(fail(path, e)); box.replaceChildren(h('div', { class: 'small', style: 'color: var(--bad)' }, `This part of the screen failed: ${e.message}`)); });
      return box;
    }

    // A front page (spec.page "front") is drawn edge to edge, its parts in
    // sections; its blocks work in any screen.
    const front = window.__sa_front ? window.__sa_front({ h, val, T, block, run, app, scope }) : null;
    const isFront = spec.page === 'front' && !!front;
    if (isFront) document.body.classList.add('front');
    // The screen's page structure (engine/quality.mjs LAYOUTS): a class the
    // kit draws (kit.css, front.css), and for a working screen its parts
    // arranged: the title and numbers as its head, then the screen's groups
    // (each starts at a heading or a row of numbers) placed as the layout says.
    const LAYOUTS = ['hero', 'split', 'editorial', 'dashboard', 'gallery', 'tool'];
    const layout = LAYOUTS.includes(spec.layout) ? spec.layout : null;
    if (layout) { document.body.classList.add(`layout-${layout}`); document.documentElement.dataset.layout = layout; }
    const flatNodes = (x) => (Array.isArray(x) ? x.flatMap(flatNodes) : x instanceof Node ? [x] : []);
    const headLike = (el) => el.nodeType === 1 && (el.matches('h1, h2, .say, .kpis, .hero-band') || (el.matches('.row') && !!el.querySelector(':scope > h1, :scope > h2')));
    function arrange(els) {
      const list = flatNodes(els);
      if (!layout || list.length < 2) return list;
      const head = [];
      let i = 0;
      while (i < list.length && (headLike(list[i]) || (list[i].nodeType === 1 && list[i].matches('p') && head.length))) head.push(list[i++]);
      const groups = [];
      for (; i < list.length; i++) {
        if (!groups.length || (headLike(list[i]) && groups.at(-1).length)) groups.push([]);
        groups.at(-1).push(list[i]);
      }
      const box = (cls, kids) => h('div', { class: cls }, kids);
      const top = head.length ? [box(`lay-head${layout === 'hero' ? ' lay-hero' : ''}`, head)] : [];
      if (!groups.length) return top;
      const cells = groups.map((g, k) => box(`lay-group lay-g${k}`, g));
      if (layout === 'split' && cells.length > 1) return [...top, box('lay-split', [box('lay-main', [cells[0]]), box('lay-aside', cells.slice(1))])];
      if (layout === 'tool') return [...top, box('lay-main lay-panel', [cells[0]]), ...cells.slice(1)];
      if (layout === 'dashboard') return [...top, box('lay-grid', cells)];
      return [...top, ...cells];
    }

    // ---- drawing: the whole screen again, keeping focus and what is typed.
    function focusPath() {
      const a = document.activeElement;
      if (!a || a === document.body || !root.contains(a)) return null;
      const idx = [];
      for (let n = a; n && n !== root; n = n.parentNode) idx.unshift([...n.parentNode.children].indexOf(n));
      return { idx, start: a.selectionStart, end: a.selectionEnd, tag: a.tagName };
    }
    function restore(f) {
      if (!f) return;
      let n = root;
      for (const i of f.idx) { n = n && n.children[i]; }
      if (n && n.tagName === f.tag && n !== document.activeElement) {
        n.focus();
        try { if (f.start != null) n.setSelectionRange(f.start, f.end); } catch { /* not a text field */ }
      }
    }
    function draw() {
      if (drawing) { again = true; return; }
      drawing = true;
      try {
        do {
          again = false;
          const f = focusPath();
          heroAt = null;
          if (isFront) {
            root.replaceChildren(h('div', { class: `screen front${layout ? ` layout-${layout}` : ''}` }, front.page(spec.body, scope(), layout)));
          } else root.replaceChildren(h('div', { class: `col screen${layout ? ` layout-${layout}` : ''}` }, arrange(blocks(spec.body, scope(), 'body'))));
          if (front) front.after(root);
          restore(f);
        } while (again);
      } finally { drawing = false; }
      // A form this draw no longer shows was closed.
      for (const [p, f] of forms) if (!f.el || !f.el.isConnected) { abandon(f); forms.delete(p); }
    }

    window.__sa_relook = () => draw();
    phone.addEventListener('change', () => { if (lookIs() !== 'plain') draw(); });
    state = { ...(val(spec.state || {}, scope(), 'state') || {}) };
    await load();
    compute();
    draw();
    app.onChange('*', () => refresh());
  };
})();
