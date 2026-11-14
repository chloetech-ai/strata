// Front pages: the blocks a site's public pages are made of, for screens
// (web/screen.js). A screen with "page": "front" is drawn edge to edge: a
// site header, a hero, carousels, galleries, features, numbers, quotes,
// bands of color or photo, pricing, questions and a footer, each a block
// written as data like the rest. Motion is CSS (moving gradients, a slow
// zoom on photos, parts that rise into view as the page scrolls), quiet by
// default, off when the device asks for less motion or the design says
// "still". Styles: web/front.css.
(() => {
  // Line icons, 24 x 24, drawn with the text color.
  const I = {
    check: '<path d="M5 12.5l4.5 4.5L19 7.5"/>',
    star: '<path d="M12 3.5l2.6 5.4 5.9.8-4.3 4.1 1 5.8L12 16.8l-5.2 2.8 1-5.8-4.3-4.1 5.9-.8z"/>',
    heart: '<path d="M12 20s-7.5-4.6-9.3-9.2C1.5 7.6 3.6 4.5 6.9 4.5c2.1 0 3.6 1.2 5.1 3 1.5-1.8 3-3 5.1-3 3.3 0 5.4 3.1 4.2 6.3C19.5 15.4 12 20 12 20z"/>',
    bolt: '<path d="M13 2.5L4.5 13.5h6.5l-1 8 8.5-11h-6.5z"/>',
    shield: '<path d="M12 3l7.5 3v5.5c0 4.8-3.2 8.2-7.5 9.5-4.3-1.3-7.5-4.7-7.5-9.5V6z"/><path d="M9 12l2 2 4-4"/>',
    clock: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3.5 2"/>',
    leaf: '<path d="M5 19c0-8.5 6.2-14 15-14 0 8.8-5.5 15-14 15"/><path d="M5 19l7.5-7.5"/>',
    sparkles: '<path d="M11 3l1.9 5.1L18 10l-5.1 1.9L11 17l-1.9-5.1L4 10l5.1-1.9z"/><path d="M18.5 15l.8 2.2 2.2.8-2.2.8-.8 2.2-.8-2.2-2.2-.8 2.2-.8z"/>',
    chat: '<path d="M4 5.5h16v10.5H9.5L5 20v-4H4z"/>',
    calendar: '<rect x="3.5" y="5" width="17" height="15.5" rx="2.5"/><path d="M3.5 10h17M8 3v4M16 3v4"/>',
    pin: '<path d="M12 21s-7-6.1-7-11.2a7 7 0 0114 0C19 14.9 12 21 12 21z"/><circle cx="12" cy="9.8" r="2.5"/>',
    truck: '<path d="M2.5 6.5h11.5v9.5H2.5zM14 9.5h4.2l3.3 3.3v3.2H14"/><circle cx="6.5" cy="17.5" r="2"/><circle cx="17.5" cy="17.5" r="2"/>',
    gift: '<rect x="3" y="8" width="18" height="4.5" rx="1"/><path d="M5 12.5V21h14v-8.5M12 8v13M12 8S10.5 3.5 8 4.4 9 8 12 8zm0 0s1.5-4.5 4-3.6S15 8 12 8z"/>',
    camera: '<path d="M3.5 8h3.5l2-3h6l2 3h3.5v11.5h-17z"/><circle cx="12" cy="13.3" r="3.6"/>',
    music: '<path d="M9 18V5.5l11-2v12.5"/><circle cx="6.5" cy="18" r="2.5"/><circle cx="17.5" cy="16" r="2.5"/>',
    cup: '<path d="M4 8.5h13v4.5a6 6 0 01-6 6h-1a6 6 0 01-6-6zM17 9.5h1.5a2.5 2.5 0 010 5H17M8.5 3.5v2M12.5 3.5v2"/>',
    globe: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c3.2 3.6 3.2 14.4 0 18M12 3c-3.2 3.6-3.2 14.4 0 18"/>',
    lock: '<rect x="5" y="10.5" width="14" height="10" rx="2.5"/><path d="M8 10.5V8a4 4 0 018 0v2.5"/>',
    users: '<circle cx="9" cy="8" r="3.5"/><path d="M2.5 20c.6-3.9 3.2-6 6.5-6s5.9 2.1 6.5 6"/><path d="M16 4.6a3.5 3.5 0 010 6.8M18 14.2c2 .7 3.2 2.6 3.5 5.8"/>',
    chart: '<path d="M4 20V11M10 20V4.5M16 20v-6.5M2.5 20.5h19"/>',
    home: '<path d="M3.5 10.5L12 4l8.5 6.5V20h-5.5v-5.5h-6V20H3.5z"/>',
    phone: '<rect x="7" y="2.5" width="10" height="19" rx="2.5"/><path d="M11 18h2"/>',
    mail: '<rect x="3" y="5" width="18" height="14" rx="2.5"/><path d="M3.5 7l8.5 6 8.5-6"/>',
    tag: '<path d="M3 12.5V3.5h9l9 9-9 9z"/><circle cx="7.5" cy="8" r="1.5"/>',
    cart: '<path d="M3 4h2.5l2.3 10.5h10.7L21 7.5H6.4"/><circle cx="9.5" cy="19" r="1.5"/><circle cx="17" cy="19" r="1.5"/>',
    sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2.5v2M12 19.5v2M4.6 4.6l1.4 1.4M18 18l1.4 1.4M2.5 12h2M19.5 12h2M4.6 19.4L6 18M18 6l1.4-1.4"/>',
    moon: '<path d="M20 14.5A8 8 0 019.5 4a8.2 8.2 0 1010.5 10.5z"/>',
    book: '<path d="M12 7a3 3 0 00-3-3H3v15h6a3 3 0 013 3zM12 7a3 3 0 013-3h6v15h-6a3 3 0 00-3 3z"/>',
    flame: '<path d="M12 21.5c4 0 7-2.8 7-6.9 0-4-2.8-6-4-9.1-1 2-2 3-3 3 0-2-1-4.2-3-6 0 4-4 6.2-4 12.1 0 4.1 3 6.9 7 6.9z"/>',
    award: '<circle cx="12" cy="9" r="6"/><path d="M8.6 14L7 22l5-2.8 5 2.8-1.6-8"/>',
    smile: '<circle cx="12" cy="12" r="9"/><path d="M8 14s1.5 2 4 2 4-2 4-2M9 9.5h.01M15 9.5h.01"/>',
    card: '<rect x="2.5" y="5" width="19" height="14" rx="2.5"/><path d="M2.5 10h19M6 15h4"/>',
    wifi: '<path d="M2.5 8.8a14.5 14.5 0 0119 0M5.5 12.2a10 10 0 0113 0M8.8 15.6a5 5 0 016.4 0M12 19h.01"/>',
    arrow: '<path d="M5 12h14M13 6l6 6-6 6"/>',
    plus: '<path d="M12 5v14M5 12h14"/>',
    play: '<path d="M7.5 4.5L19.5 12 7.5 19.5z"/>',
    dumbbell: '<path d="M6.5 7v10M3.5 9.5v5M17.5 7v10M20.5 9.5v5M6.5 12h11"/>',
    briefcase: '<rect x="3" y="7" width="18" height="13" rx="2.5"/><path d="M9 7V5.5A1.5 1.5 0 0110.5 4h3A1.5 1.5 0 0115 5.5V7M3 13h18"/>',
    code: '<path d="M8.5 6.5L3 12l5.5 5.5M15.5 6.5L21 12l-5.5 5.5"/>',
    cloud: '<path d="M7 18.5a5 5 0 01-.6-9.96A6 6 0 0117.8 9 4.75 4.75 0 0117.5 18.5z"/>',
    search: '<circle cx="11" cy="11" r="7"/><path d="M20.5 20.5l-4.5-4.5"/>',
    bell: '<path d="M6 16.5V11a6 6 0 0112 0v5.5l1.8 1.8H4.2zM10 21h4"/>',
    rocket: '<path d="M12.5 15.5l-4-4c1.4-5.4 5.3-8.6 12-9-.4 6.7-3.6 10.6-8 13zM8.5 11.5l-4 .8 2.3-3.8h4M12.5 15.5l-.8 4 3.8-2.3v-4"/><circle cx="15.5" cy="8.5" r="1.5"/>',
    scissors: '<circle cx="6" cy="6.5" r="2.8"/><circle cx="6" cy="17.5" r="2.8"/><path d="M8.3 8.2L20 19M8.3 15.8L20 5"/>',
    paw: '<circle cx="7" cy="9" r="1.8"/><circle cx="11" cy="6" r="1.8"/><circle cx="15.5" cy="7" r="1.8"/><circle cx="18" cy="11" r="1.8"/><path d="M12 12c-3 0-5.5 3.5-5.5 6 0 1.5 1.2 2.3 2.6 2 1.2-.3 1.9-.8 2.9-.8s1.7.5 2.9.8c1.4.3 2.6-.5 2.6-2 0-2.5-2.5-6-5.5-6z"/>',
    tooth: '<path d="M7 4c-2.5 0-4 2-3.5 5 .4 2.4 1.5 3.5 2 6 .4 2 .6 5 2 5 1.6 0 1.5-4 2.5-5.5.5-.8 1.5-.8 2 0 1 1.5.9 5.5 2.5 5.5 1.4 0 1.6-3 2-5 .5-2.5 1.6-3.6 2-6 .5-3-1-5-3.5-5-2 0-3 1-5 1S9 4 7 4z"/>',
    plane: '<path d="M21 15.5l-8-4.5V5a1.5 1.5 0 00-3 0v6l-8 4.5v2l8-2.5v4.5l-2.5 1.8V22l4-1 4 1v-1.7L13 18.5V14l8 2.5z"/>',
    palette: '<path d="M12 3a9 9 0 100 18c1.2 0 1.8-.8 1.8-1.7 0-1.1-.9-1.6-.9-2.6 0-1 .8-1.7 1.9-1.7h2.2A3.9 3.9 0 0021 11.1C21 6.6 17 3 12 3z"/><circle cx="7.5" cy="11" r="1.2"/><circle cx="10.5" cy="7" r="1.2"/><circle cx="15.5" cy="7.5" r="1.2"/>',
  };
  const NS = 'http://www.w3.org/2000/svg';
  const icon = (name, cls = 'fp-ico') => {
    const svg = document.createElementNS(NS, 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('class', cls);
    svg.setAttribute('aria-hidden', 'true');
    svg.innerHTML = I[name] || I.sparkles;
    return svg;
  };

  const FULL = new Set(['nav', 'hero', 'band', 'marquee', 'footer']);
  const SECTION = new Set(['features', 'stats', 'quotes', 'gallery', 'pricing', 'faq', 'steps', 'media', 'carousel']);
  const KINDS = [...FULL, ...SECTION];
  window.__sa_front_kinds = KINDS;

  window.__sa_front = (ctx) => {
    const { h, val, T, block, run, app, scope } = ctx;
    // What a part keeps across redraws (a carousel's slide, an open answer),
    // and the parts already shown (they do not rise into view again).
    const keep = new Map();
    const seen = new Set();
    const played = new Set();
    const root = document.documentElement;
    const css = (name) => getComputedStyle(root).getPropertyValue(name).trim();
    const still = () => matchMedia('(prefers-reduced-motion: reduce)').matches || css('--play') === 'paused';

    // "Coffee, *slowly*" draws "slowly" in the design's gradient.
    const rich = (text) => String(text ?? '').split(/\*([^*]+)\*/).map((p, i) => (i % 2 ? h('em', { class: 'fp-em' }, p) : p));
    const tx = (v, s, path) => (v === undefined || v === null ? '' : T(v, s, path));
    const list = (v, s, path) => { const x = val(v, s, path); return Array.isArray(x) ? x : x == null ? [] : [x]; };

    const scrollToId = (id) => {
      const el = document.getElementById(id);
      if (el) el.scrollIntoView({ behavior: still() ? 'auto' : 'smooth', block: 'start' });
    };
    // An action: {label, open: "view", params, href: "#part" or a URL, do: "=handler", style: "primary" | "secondary" | "link"}.
    const action = (a, s, path, i = 0) => {
      if (a == null || a === false) return null;
      if (typeof a === 'string') a = { label: a };
      const style = a.style || a.class || (i === 0 ? 'primary' : 'secondary');
      const label = tx(a.label, s, path);
      const kids = [label, style === 'link' ? h('span', { class: 'fp-chev', 'aria-hidden': 'true' }, '›') : null];
      const cls = `fp-btn ${style}`;
      const href = a.href !== undefined ? tx(a.href, s, path) : '';
      if (href && /^https?:/i.test(href)) return h('a', { class: cls, href, target: '_blank', rel: 'noopener' }, kids);
      const go = async () => {
        if (href && href.startsWith('#')) { scrollToId(href.slice(1)); return; }
        if (a.open !== undefined) { app.open(tx(a.open, s, path), a.params ? val(a.params, s, path) : undefined); return; }
        if (a.do !== undefined) { const fn = val(a.do, s, `${path}.do`); if (typeof fn === 'function') await run(fn, [], `${path}.do`); }
      };
      return h('button', { class: cls, type: 'button', onclick: (e) => { e.stopPropagation(); go(); } }, kids);
    };
    const actions = (b, s, path) => {
      const xs = b.actions !== undefined ? list(b.actions, s, `${path}.actions`) : b.action !== undefined ? [val(b.action, s, `${path}.action`)] : [];
      const out = xs.map((a, i) => action(a, s, `${path}.actions[${i}]`, i)).filter(Boolean);
      return out.length ? h('div', { class: 'fp-actions' }, out) : null;
    };

    // A photo: a URL or a stored file's id in "src"; without one (a preview,
    // or nothing found), the design's gradient stands in.
    const pic = (o, s, path, cls = '', eager = false) => {
      const src = o && o.src !== undefined ? val(o.src, s, `${path}.src`) : null;
      const alt = String((o && (o.alt || o.photo || o.title)) || '');
      if (!src || typeof src !== 'string') return h('div', { class: `fp-ph ${cls}`, role: 'img', 'aria-label': alt });
      const img = h('img', { src: app.fileUrl(src), alt, class: cls || null, loading: eager ? 'eager' : 'lazy', decoding: 'async' });
      img.addEventListener('error', () => img.replaceWith(h('div', { class: `fp-ph ${cls}` })), { once: true });
      return img;
    };
    const credit = (o) => (o && o.credit ? (o.credit_url ? h('a', { class: 'fp-credit', href: o.credit_url, target: '_blank', rel: 'noopener' }, o.credit) : h('span', { class: 'fp-credit' }, o.credit)) : null);

    // A section's heading: an eyebrow, a title and a line under it.
    const head = (b, s, path) => {
      if (!b.title && !b.eyebrow && !b.sub) return null;
      return h('div', { class: `fp-head${b.align === 'left' ? ' left' : ''}` },
        b.eyebrow ? h('p', { class: 'fp-eyebrow' }, tx(b.eyebrow, s, path)) : null,
        b.title ? h('h2', { class: 'fp-title' }, rich(tx(b.title, s, path))) : null,
        b.sub ? h('p', { class: 'fp-sub' }, tx(b.sub, s, path)) : null);
    };
    // A part that rises into view, once.
    const rv = (el, key, i = 0) => { el.classList.add('rv'); el.dataset.rk = key; el.style.setProperty('--i', String(i % 8)); if (seen.has(key)) el.classList.add('in'); return el; };
    const blobs = () => h('div', { class: 'fp-blobs', 'aria-hidden': 'true' }, h('i'), h('i'), h('i'), h('b', { class: 'fp-grain' }));

    const F = {};

    F.nav = (b, s, path) => {
      const links = list(b.links, s, `${path}.links`);
      let el;
      const close = () => el && el.classList.remove('open');
      el = h('header', { class: 'fp-nav' }, h('div', { class: 'fp-nav-in' },
        h('a', { class: 'fp-brand', href: '#', onclick: (e) => { e.preventDefault(); scrollTo({ top: 0, behavior: still() ? 'auto' : 'smooth' }); } }, h('span', { class: 'fp-mark', 'aria-hidden': 'true' }), tx(b.nav, s, path)),
        h('nav', { class: 'fp-links' }, links.map((l, i) => {
          const a = action(typeof l === 'string' ? { label: l } : { ...l, style: 'navlink' }, s, `${path}.links[${i}]`, 1);
          a && a.addEventListener('click', close);
          return a;
        })),
        b.action !== undefined ? action(val(b.action, s, `${path}.action`), s, `${path}.action`, 0) : null,
        h('button', { class: 'fp-menu', type: 'button', 'aria-label': 'Menu', onclick: () => el.classList.toggle('open') }, h('i'), h('i'))));
      return el;
    };

    // The page's layout (a screen's "layout") chooses how a hero that does
    // not say its look is drawn: a photo, words beside it, or plain words.
    let pageLayout = null;
    const LAYOUT_HERO = { hero: 'photo', split: 'split', editorial: 'plain', gallery: 'split', dashboard: 'split', tool: 'split' };
    F.hero = (b, s, path) => {
      const pic0 = b.src || b.photo;
      const look = b.look || (pageLayout && LAYOUT_HERO[pageLayout] && (pic0 || LAYOUT_HERO[pageLayout] === 'plain') ? LAYOUT_HERO[pageLayout] : pic0 ? 'photo' : 'gradient');
      const align = b.align || (look === 'split' || pageLayout === 'editorial' ? 'left' : 'center');
      const height = b.height || (look === 'photo' ? 'full' : look === 'split' ? 'tall' : 'tall');
      const text = h('div', { class: `fp-hero-text ${align}` },
        [b.eyebrow ? h('p', { class: 'fp-eyebrow' }, tx(b.eyebrow, s, path)) : null,
          h('h1', { class: 'fp-display' }, rich(tx(b.hero, s, path))),
          b.sub ? h('p', { class: 'fp-lead' }, tx(b.sub, s, path)) : null,
          actions(b, s, path),
          b.note ? h('p', { class: 'fp-note' }, tx(b.note, s, path)) : null].filter(Boolean).map((x, k) => { x.style.setProperty('--k', String(k)); return x; }));
      const cls = ['fp-hero', `look-${look}`, `h-${height}`, look === 'photo' ? 'fp-dark' : '', played.has(path) ? 'played' : ''].filter(Boolean).join(' ');
      played.add(path);
      if (look === 'photo') {
        return h('section', { class: cls, id: b.id || null },
          h('div', { class: 'fp-bg', 'data-parallax': '0.35' }, pic(b, s, path, 'kb', true)),
          h('div', { class: `fp-scrim ${align}` }),
          h('div', { class: 'fp-wrap' }, text), credit(b));
      }
      if (look === 'split') {
        return h('section', { class: cls, id: b.id || null }, blobs(),
          h('div', { class: 'fp-wrap fp-split' }, text,
            h('figure', { class: 'fp-hero-pic' }, pic(b, s, path, '', true), credit(b),
              b.badge ? h('div', { class: 'fp-float' }, rich(tx(b.badge, s, path))) : null)));
      }
      return h('section', { class: cls, id: b.id || null }, look === 'gradient' ? blobs() : null,
        h('div', { class: 'fp-wrap' }, text,
          b.src || b.photo ? h('figure', { class: 'fp-hero-shot' }, pic(b, s, path, '', true), credit(b)) : null));
    };

    F.band = (b, s, path) => {
      const look = b.look || (b.src || b.photo ? 'photo' : 'gradient');
      const dark = look === 'photo' || look === 'accent' || look === 'dark';
      return h('section', { class: `fp-band look-${look}${dark ? ' fp-dark' : ''}${b.align === 'left' ? ' left' : ''}`, id: b.id || null },
        look === 'photo' ? h('div', { class: 'fp-bg', 'data-parallax': '0.18' }, pic(b, s, path, '')) : null,
        look === 'photo' ? h('div', { class: 'fp-scrim' }) : null,
        look === 'gradient' ? blobs() : null,
        look === 'dark' ? h('div', { class: 'fp-glow', 'aria-hidden': 'true' }) : null,
        h('div', { class: 'fp-wrap fp-band-in' }, rv(h('div', { class: 'fp-band-body' }, head(b, s, path), block({ col: Array.isArray(b.band) ? b.band : [b.band] }, s, `${path}.band`), actions(b, s, path)), path)),
        look === 'photo' ? credit(b) : null);
    };

    F.media = (b, s, path) => {
      const side = b.side === 'right' ? 'right' : 'left';
      return h('div', { class: `fp-media side-${side}` },
        rv(h('figure', { class: 'fp-media-pic' }, pic(b, s, path), credit(b)), `${path}.pic`),
        rv(h('div', { class: 'fp-media-text' },
          b.eyebrow ? h('p', { class: 'fp-eyebrow' }, tx(b.eyebrow, s, path)) : null,
          b.title ? h('h2', { class: 'fp-title' }, rich(tx(b.title, s, path))) : null,
          b.sub ? h('p', { class: 'fp-sub' }, tx(b.sub, s, path)) : null,
          block({ col: Array.isArray(b.media) ? b.media : [b.media] }, s, `${path}.media`),
          actions(b, s, path)), `${path}.text`, 1));
    };

    const iconOf = (name) => {
      const n = String(name || '');
      return I[n] ? h('span', { class: 'fp-icon' }, icon(n)) : n ? h('span', { class: 'fp-icon emoji' }, n) : null;
    };

    // Items of a list block: as given, or each made from "item" (a template
    // evaluated with the row as "as", default "item").
    const items = (b, key, s, path) => {
      const raw = list(b[key], s, `${path}.${key}`);
      if (b.item === undefined) return raw.map((x) => (typeof x === 'string' ? { title: x } : x));
      return raw.map((x, i) => val(b.item, scope({ ...s, [b.as || 'item']: x, index: i }), `${path}.item`));
    };

    F.features = (b, s, path) => {
      const xs = items(b, 'features', s, path);
      const look = b.look || (xs.some((x) => x && (x.src || x.photo)) ? 'photos' : 'tiles');
      return [head(b, s, path), h('div', { class: `fp-features look-${look}${b.columns ? ` cols-${b.columns}` : ''}` }, xs.map((f, i) => rv(h('div', { class: 'fp-feature' },
        look === 'photos' ? h('figure', { class: 'fp-feature-pic' }, pic(f, s, `${path}.features[${i}]`)) : iconOf(f.icon),
        h('h3', null, rich(f.title ?? '')), f.text ? h('p', { class: 'muted' }, String(f.text)) : null,
        f.action ? action(f.action, s, `${path}.features[${i}].action`, 1) : null), `${path}[${i}]`, i)))];
    };

    F.stats = (b, s, path) => {
      const xs = items(b, 'stats', s, path);
      return [head(b, s, path), h('div', { class: 'fp-stats' }, xs.map((x, i) => {
        const v = x.value;
        const num = typeof v === 'number' || (typeof v === 'string' && /^-?\d+(\.\d+)?$/.test(v));
        const shown = `${x.prefix ?? ''}${num ? Number(v).toLocaleString() : v ?? ''}${x.suffix ?? ''}`;
        return rv(h('div', { class: 'fp-stat' },
          h('div', { class: 'fp-stat-v', ...(num ? { 'data-count': String(v), 'data-prefix': x.prefix ?? '', 'data-suffix': x.suffix ?? '' } : {}) }, shown),
          h('div', { class: 'fp-stat-l' }, String(x.label ?? '')), x.hint ? h('div', { class: 'small muted' }, String(x.hint)) : null), `${path}[${i}]`, i);
      }))];
    };

    const initials = (name) => String(name || '?').split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join('');
    const quote = (q, s, path) => h('figure', { class: 'fp-quote' },
      q.stars ? h('div', { class: 'fp-stars', 'aria-label': `${q.stars} of 5` }, '★'.repeat(Math.round(q.stars))) : null,
      h('blockquote', null, rich(q.text ?? q.title ?? '')),
      h('figcaption', null, q.src ? pic(q, s, path, 'fp-av') : h('span', { class: 'fp-av fp-init' }, initials(q.name)),
        h('div', null, h('b', null, String(q.name ?? '')), q.role ? h('span', { class: 'muted small' }, String(q.role)) : null)));

    // A row of cards that scrolls sideways, with arrows (Apple's shelves).
    const shelf = (cards, b, path) => {
      const st = keep.get(path) || { x: 0 };
      keep.set(path, st);
      const track = h('div', { class: 'fp-track', onscroll: () => { st.x = track.scrollLeft; } }, cards);
      const by = (d) => track.scrollBy({ left: d * Math.max(260, track.clientWidth * 0.8), behavior: still() ? 'auto' : 'smooth' });
      requestAnimationFrame(() => { if (st.x) track.scrollLeft = st.x; });
      return h('div', { class: 'fp-carousel cards' }, track,
        cards.length > 1 ? h('div', { class: 'fp-ctrl' },
          h('button', { class: 'fp-round', type: 'button', 'aria-label': 'Back', onclick: () => by(-1) }, icon('arrow', 'fp-ico flip')),
          h('button', { class: 'fp-round', type: 'button', 'aria-label': 'More', onclick: () => by(1) }, icon('arrow'))) : null);
    };

    F.quotes = (b, s, path) => {
      const xs = items(b, 'quotes', s, path);
      const look = b.look || (xs.length > 3 ? 'carousel' : 'grid');
      const cards = xs.map((q, i) => quote(q, s, `${path}.quotes[${i}]`));
      return [head(b, s, path), look === 'carousel' ? shelf(cards, b, path) : h('div', { class: 'fp-quotes' }, cards.map((c, i) => rv(c, `${path}[${i}]`, i)))];
    };

    // A card; in a row where other cards have photos, one without gets the
    // design's gradient with its initials, so the row stays even.
    const card = (it, s, path, row = false) => {
      const go = it.action && (it.action.open !== undefined || it.action.href !== undefined) ? action(it.action, s, `${path}.action`, 1) : null;
      const has = it.src || it.photo;
      const ph = () => h('div', { class: 'fp-ph', role: 'img', 'aria-label': String(it.title || '') }, h('span', { class: 'fp-ph-init' }, initials(it.title)));
      const el = h('div', { class: `fp-card${has || row ? ' has-pic' : ''}` },
        has || row ? h('figure', { class: 'fp-card-pic' }, has ? pic(it, s, path) : ph(), it.badge ? h('span', { class: 'fp-badge' }, String(it.badge)) : null, credit(it)) : null,
        h('div', { class: 'fp-card-body' },
          it.eyebrow ? h('p', { class: 'fp-eyebrow small' }, String(it.eyebrow)) : null,
          it.title ? h('h3', null, rich(it.title)) : null,
          it.text ? h('p', { class: 'muted' }, String(it.text)) : null,
          it.price !== undefined || it.action ? h('div', { class: 'fp-card-foot' }, it.price !== undefined ? h('span', { class: 'fp-price' }, String(it.price)) : null, it.action ? action(it.action, s, `${path}.action`, it.action.style ? 1 : 0) : null) : null));
      if (go) { el.classList.add('link'); el.addEventListener('click', () => go.click()); }
      return el;
    };

    // Slides: one photo at a time across the page, captions rising in, moving
    // on by themselves (paused while pointed at), by arrows, dots, swipes
    // and the arrow keys.
    const slides = (xs, b, s, path) => {
      const st = keep.get(path) || { i: 0 };
      keep.set(path, st);
      const n = xs.length;
      if (st.i >= n) st.i = 0;
      const secs = b.auto === undefined ? 6 : Number(b.auto) || 0;
      const slideEls = xs.map((it, k) => h('div', { class: `fp-slide${k === st.i ? ' on' : ''}`, 'aria-hidden': k === st.i ? 'false' : 'true' },
        pic(it, s, `${path}[${k}]`, '', k === 0), h('div', { class: 'fp-scrim' }),
        h('div', { class: 'fp-wrap fp-slide-cap' },
          it.eyebrow ? h('p', { class: 'fp-eyebrow' }, String(it.eyebrow)) : null,
          it.title ? h('h2', { class: 'fp-title' }, rich(it.title)) : null,
          it.text ? h('p', { class: 'fp-lead' }, String(it.text)) : null,
          it.action ? h('div', { class: 'fp-actions' }, action(it.action, s, `${path}[${k}].action`, 0)) : null), credit(it)));
      const dots = xs.map((_, k) => h('button', { class: `fp-dot${k === st.i ? ' on' : ''}`, type: 'button', 'aria-label': `Slide ${k + 1}`, onclick: () => go(k) }, h('i')));
      const el = h('div', { class: 'fp-carousel slides fp-dark', tabindex: '0', style: `--dur: ${secs || 6}s` }, h('div', { class: 'fp-slides' }, slideEls),
        n > 1 ? h('button', { class: 'fp-arrow prev', type: 'button', 'aria-label': 'Back', onclick: () => go(st.i - 1) }, icon('arrow', 'fp-ico flip')) : null,
        n > 1 ? h('button', { class: 'fp-arrow next', type: 'button', 'aria-label': 'Next', onclick: () => go(st.i + 1) }, icon('arrow')) : null,
        n > 1 ? h('div', { class: 'fp-dots' }, dots) : null);
      let timer = null;
      const arm = () => {
        clearTimeout(timer);
        if (!secs || n < 2 || still()) return;
        timer = setTimeout(() => { if (!el.isConnected) return; if (el.classList.contains('paused') || document.hidden) arm(); else go(st.i + 1); }, secs * 1000);
      };
      function go(k) {
        st.i = (k + n) % n;
        slideEls.forEach((x, j) => { x.classList.toggle('on', j === st.i); x.setAttribute('aria-hidden', j === st.i ? 'false' : 'true'); });
        // the running dot starts again from empty
        dots.forEach((d, j) => { d.classList.remove('on'); if (j === st.i) { void d.offsetWidth; d.classList.add('on'); } });
        arm();
      }
      el.addEventListener('pointerenter', () => el.classList.add('paused'));
      el.addEventListener('pointerleave', () => { el.classList.remove('paused'); });
      el.addEventListener('keydown', (e) => { if (e.key === 'ArrowLeft') go(st.i - 1); if (e.key === 'ArrowRight') go(st.i + 1); });
      let x0 = null;
      el.addEventListener('pointerdown', (e) => { x0 = e.clientX; });
      el.addEventListener('pointerup', (e) => { if (x0 !== null && Math.abs(e.clientX - x0) > 40) go(st.i + (e.clientX < x0 ? 1 : -1)); x0 = null; });
      arm();
      return el;
    };

    F.carousel = (b, s, path) => {
      const look = b.look || 'cards';
      if (look === 'slides') return slides(items(b, 'carousel', s, path), b, s, path);
      let cards;
      if (b.body !== undefined) {
        cards = list(b.carousel, s, `${path}.carousel`).map((x, i) => h('div', { class: 'fp-card fp-card-body' }, block({ col: b.body }, scope({ ...s, [b.as || 'item']: x, index: i }), `${path}.body`)));
      } else {
        const xs = items(b, 'carousel', s, path);
        const row = xs.some((it) => it && (it.src || it.photo));
        cards = xs.map((it, i) => card(it || {}, s, `${path}[${i}]`, row));
      }
      if (!cards.length) return [head(b, s, path), h('div', { class: 'empty' }, tx(b.empty ?? 'Nothing here yet.', s, path))];
      return [head(b, s, path), shelf(cards, b, path)];
    };

    // A photo seen large, over the page: arrows, the keys, a tap outside closes.
    const lightbox = (xs, k, s, path) => {
      let i = k;
      const img = h('div', { class: 'fp-lb-pic' });
      const cap = h('div', { class: 'fp-lb-cap' });
      const show = () => { img.replaceChildren(pic(xs[i], s, `${path}[${i}]`, '', true)); cap.replaceChildren(xs[i].title ? h('b', null, String(xs[i].title)) : '', xs[i].text ? ` ${xs[i].text}` : ''); };
      const close = () => { box.remove(); removeEventListener('keydown', key); };
      const key = (e) => { if (e.key === 'Escape') close(); if (e.key === 'ArrowLeft') { i = (i - 1 + xs.length) % xs.length; show(); } if (e.key === 'ArrowRight') { i = (i + 1) % xs.length; show(); } };
      const box = h('div', { class: 'fp-lb', onclick: (e) => { if (e.target === box) close(); } }, img, cap,
        h('button', { class: 'fp-round close', type: 'button', 'aria-label': 'Close', onclick: close }, icon('plus', 'fp-ico x')),
        xs.length > 1 ? h('button', { class: 'fp-arrow prev', type: 'button', 'aria-label': 'Back', onclick: () => key({ key: 'ArrowLeft' }) }, icon('arrow', 'fp-ico flip')) : null,
        xs.length > 1 ? h('button', { class: 'fp-arrow next', type: 'button', 'aria-label': 'Next', onclick: () => key({ key: 'ArrowRight' }) }, icon('arrow')) : null);
      addEventListener('keydown', key);
      show();
      document.body.append(box);
    };

    F.gallery = (b, s, path) => {
      const xs = items(b, 'gallery', s, path).map((x) => (typeof x === 'string' ? { src: x } : x || {}));
      const look = b.look || 'mosaic';
      // A mosaic: the first photo large, then as many wide ones as it takes
      // to end on a full row (of four columns, or two on a phone).
      const size = xs.map(() => '');
      if (look === 'mosaic' && xs.length >= 3) {
        size[0] = 'big';
        let need = (4 - ((xs.length + 3) % 4)) % 4;
        for (let k = xs.length - 1; k > 0 && need > 0; k--, need--) size[k] = 'wide';
      } else if (look === 'mosaic') size.fill('wide');
      return [head(b, s, path), h('div', { class: `fp-gallery look-${look}` }, xs.map((it, i) => rv(h('figure', { class: `fp-gitem ${size[i]}`.trim(), onclick: () => lightbox(xs, i, s, path) },
        pic(it, s, `${path}[${i}]`), it.title ? h('figcaption', null, String(it.title)) : null, credit(it)), `${path}[${i}]`, i)))];
    };

    F.marquee = (b, s, path) => {
      const xs = list(b.marquee, s, `${path}.marquee`);
      const one = (hidden) => xs.map((x) => h('span', { class: 'fp-mq-item', 'aria-hidden': hidden ? 'true' : null },
        typeof x === 'string' ? rich(x) : x && x.src ? pic(x, s, path, 'fp-mq-logo') : String(x && (x.label ?? x.title) || '')));
      const secs = Math.max(18, xs.length * (Number(b.speed) || 5));
      return h('div', { class: `fp-marquee${b.look === 'big' ? ' big' : ''}`, style: `--dur: ${secs}s` }, h('div', { class: 'fp-mq-track' }, one(false), one(true)));
    };

    F.pricing = (b, s, path) => {
      const xs = items(b, 'pricing', s, path);
      return [head(b, s, path), h('div', { class: 'fp-pricing' }, xs.map((p, i) => rv(h('div', { class: `fp-plan${p.featured ? ' featured' : ''}` },
        p.featured ? h('span', { class: 'fp-plan-badge' }, String(p.badge || 'Most popular')) : null,
        h('h3', null, String(p.name ?? '')),
        h('div', { class: 'fp-price-row' }, h('span', { class: 'fp-amount' }, String(p.price ?? '')), p.per ? h('span', { class: 'fp-per' }, ` ${p.per}`) : null),
        p.text ? h('p', { class: 'muted' }, String(p.text)) : null,
        h('ul', null, (p.features || []).map((f) => h('li', null, icon('check'), h('span', null, String(f))))),
        p.action ? action({ ...(typeof p.action === 'string' ? { label: p.action } : p.action), style: p.featured ? 'primary' : 'secondary' }, s, `${path}[${i}].action`) : null), `${path}[${i}]`, i)))];
    };

    F.faq = (b, s, path) => {
      const xs = items(b, 'faq', s, path);
      return [head(b, s, path), h('div', { class: 'fp-faq' }, xs.map((q, i) => {
        const key = `${path}[${i}]`;
        const d = h('details', { class: 'fp-q', open: !!keep.get(key) }, h('summary', null, h('span', null, String(q.q ?? q.title ?? '')), h('i', { class: 'fp-plus', 'aria-hidden': 'true' })), h('div', { class: 'fp-a' }, String(q.a ?? q.text ?? '')));
        d.addEventListener('toggle', () => keep.set(key, d.open));
        return rv(d, key, i);
      }))];
    };

    F.steps = (b, s, path) => {
      const xs = items(b, 'steps', s, path);
      return [head(b, s, path), h('ol', { class: 'fp-steps' }, xs.map((x, i) => rv(h('li', { class: 'fp-step' }, h('span', { class: 'fp-step-n' }, x.icon && I[x.icon] ? icon(x.icon) : String(i + 1)), h('h3', null, String(x.title ?? '')), x.text ? h('p', { class: 'muted' }, String(x.text)) : null), `${path}[${i}]`, i)))];
    };

    F.footer = (b, s, path) => {
      const cols = list(b.columns, s, `${path}.columns`);
      const links = (xs, p) => (xs || []).map((l, i) => action(typeof l === 'string' ? { label: l, style: 'navlink' } : { ...l, style: 'navlink' }, s, `${p}[${i}]`, 1));
      return h('footer', { class: 'fp-footer', id: b.id || null }, h('div', { class: 'fp-wrap' },
        h('div', { class: 'fp-foot-top' },
          h('div', { class: 'fp-foot-brand' }, h('div', { class: 'fp-brand' }, h('span', { class: 'fp-mark', 'aria-hidden': 'true' }), tx(b.footer, s, path)), b.text ? h('p', { class: 'muted' }, tx(b.text, s, path)) : null),
          h('div', { class: 'fp-foot-cols' }, cols.map((c, i) => h('div', { class: 'fp-foot-col' }, h('h4', null, String(c.title ?? '')), links(c.links, `${path}.columns[${i}].links`))),
            b.links ? h('div', { class: 'fp-foot-col' }, links(list(b.links, s, `${path}.links`), `${path}.links`)) : null)),
        h('div', { class: 'fp-foot-bot' }, h('span', null, `© ${new Date().getFullYear()} ${tx(b.footer, s, path)}`), b.note && !/^\s*(©|\(c\)|copyright)/i.test(String(tx(b.note, s, path))) ? h('span', null, tx(b.note, s, path)) : null)));
    };

    // The page: blocks that span the width as they are, each other part in a
    // section of its own, plain blocks between them gathered in one.
    const kindOf = (b) => (b && typeof b === 'object' ? KINDS.find((k) => b[k] !== undefined) : null);
    const flat = (x) => (Array.isArray(x) ? x.flatMap(flat) : x == null || x === false ? [] : [x]);
    function page(body, s, layout = null) {
      pageLayout = layout;
      const out = [];
      let loose = null;
      const section = (b, i) => h('section', { class: `fp-sec${b && b.tint ? ' tint' : ''}`, id: (b && b.id) || null }, h('div', { class: 'fp-wrap' }));
      (Array.isArray(body) ? body : [body]).forEach((b, i) => {
        const kind = kindOf(b);
        const els = flat(block(b, s, `body[${i}]`));
        if (kind && (FULL.has(kind) || (kind === 'carousel' && b.look === 'slides'))) { loose = null; out.push(...els); return; }
        if (kind) { loose = null; const sec = section(b, i); sec.firstChild.append(...els); if (kind === 'media') sec.classList.add('media'); out.push(sec); return; }
        if (!els.length) return;
        if (!loose || (b && (b.id || b.tint))) { loose = section(b, i); out.push(loose); }
        loose.firstChild.append(...els);
      });
      // A header over a photo is white until the page scrolls under it.
      for (let i = 0; i < out.length - 1; i++) {
        if (out[i].classList?.contains('fp-nav') && (out[i + 1].classList?.contains('look-photo') || out[i + 1].classList?.contains('slides') || out[i + 1].classList?.contains('look-gradient') || out[i + 1].classList?.contains('look-split'))) {
          out[i].classList.add('over');
          if (out[i + 1].classList.contains('fp-dark')) out[i].classList.add('on-dark');
        }
      }
      return out;
    }

    // Numbers count up when they come into view.
    const countUp = (el) => {
      for (const n of el.querySelectorAll('[data-count]')) {
        const to = Number(n.dataset.count);
        if (!Number.isFinite(to) || still()) continue;
        const dec = (n.dataset.count.split('.')[1] || '').length;
        const t0 = performance.now();
        const fmt = (v) => `${n.dataset.prefix}${v.toLocaleString(undefined, { minimumFractionDigits: dec, maximumFractionDigits: dec })}${n.dataset.suffix}`;
        const step = (t) => { const p = Math.min(1, (t - t0) / 1600); n.textContent = fmt(to * (1 - (1 - p) ** 3)); if (p < 1) requestAnimationFrame(step); };
        requestAnimationFrame(step);
      }
    };
    let io = null;
    let bound = false;
    let ticking = false;
    const onScroll = () => {
      if (ticking) return;
      ticking = true;
      requestAnimationFrame(() => {
        ticking = false;
        const y = scrollY;
        for (const n of document.querySelectorAll('.fp-nav')) n.classList.toggle('solid', y > 24);
        if (still()) return;
        for (const el of document.querySelectorAll('[data-parallax]')) {
          const r = el.parentElement.getBoundingClientRect();
          if (r.bottom < 0 || r.top > innerHeight) continue;
          el.style.transform = `translate3d(0, ${(-r.top * Number(el.dataset.parallax)).toFixed(1)}px, 0)`;
        }
      });
    };
    function after(rootEl) {
      if (!io && 'IntersectionObserver' in window) {
        io = new IntersectionObserver((entries) => {
          for (const e of entries) {
            if (!e.isIntersecting) continue;
            e.target.classList.add('in');
            seen.add(e.target.dataset.rk);
            countUp(e.target);
            io.unobserve(e.target);
          }
        }, { rootMargin: '0px 0px -6% 0px', threshold: 0.08 });
      }
      for (const el of rootEl.querySelectorAll('.rv:not(.in)')) {
        if (!io || still()) { el.classList.add('in'); seen.add(el.dataset.rk); } else io.observe(el);
      }
      if (!bound) { bound = true; addEventListener('scroll', onScroll, { passive: true }); addEventListener('resize', onScroll, { passive: true }); }
      onScroll();
    }

    return { blocks: F, kinds: KINDS, page, after };
  };
})();
