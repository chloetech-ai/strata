// Workbench's preview overlay, injected into the pages of an agent's app
// that Workbench shows at /preview/<agent>/. It outlines the element under
// the pointer; a click sends the parent {type: "wb-pick", selector, text,
// html (at most 2 KB), rect, url} instead of reaching the page. The parent
// may send {type: "wb-pick-mode", on} to turn picking off and on (on by
// default in a frame, off when the page is opened on its own), and
// {type: "wb-who", id, texts?, selector?, max?, watch?}, answered with
// {type: "wb-who", id, items: [{selector, text, classes, rect}]}: the
// elements with text of their own (whose text one of `texts` holds, or that
// holds one, when given), with their classes and their parent's, so the
// parent can label what an agent changed; with `watch`, answered again
// whenever the page scrolls, is resized or changes. Messages
// are taken from the parent only, and sent to Workbench's origin only.
(() => {
  if (window.__wbOverlay) return;
  window.__wbOverlay = true;
  const framed = window.parent !== window;
  const origin = location.origin;
  let on = framed;
  let cur = null;

  const box = document.createElement("div");
  box.setAttribute("data-wb-overlay", "");
  Object.assign(box.style, {
    position: "fixed", zIndex: "2147483647", pointerEvents: "none", display: "none", boxSizing: "border-box",
    border: "2px solid #8b5cf6", borderRadius: "4px", background: "rgba(139, 92, 246, 0.10)",
  });
  const mount = () => (document.body ? document.body.appendChild(box) : document.addEventListener("DOMContentLoaded", mount, { once: true }));
  mount();

  const esc = (s) => (window.CSS && CSS.escape ? CSS.escape(s) : s.replace(/[^\w-]/g, "\\$&"));
  const rectOf = (el) => {
    const r = el.getBoundingClientRect();
    return { x: Math.round(r.left), y: Math.round(r.top), width: Math.round(r.width), height: Math.round(r.height) };
  };

  // A selector for an element: its id when unique, else a short path of
  // tags, classes and positions.
  function selector(el) {
    const parts = [];
    for (let e = el; e && e.nodeType === 1 && parts.length < 6; e = e.parentElement) {
      if (e.id && document.querySelectorAll("#" + esc(e.id)).length === 1) {
        parts.unshift("#" + esc(e.id));
        break;
      }
      let p = e.localName;
      const cls = [...e.classList].slice(0, 3);
      if (cls.length) p += "." + cls.map(esc).join(".");
      const up = e.parentElement;
      if (up) {
        const same = [...up.children].filter((c) => c.localName === e.localName);
        if (same.length > 1) p += `:nth-of-type(${same.indexOf(e) + 1})`;
      }
      parts.unshift(p);
      if (e === document.body) break;
    }
    return parts.join(" > ");
  }

  function htmlOf(el) {
    const enc = new TextEncoder();
    let h = el.outerHTML || "";
    if (enc.encode(h).length <= 2048) return h;
    h = h.slice(0, 2045);
    while (enc.encode(h).length > 2045) h = h.slice(0, -16);
    return h + "…";
  }

  function show(el) {
    if (!el || !on) {
      box.style.display = "none";
      return;
    }
    const r = el.getBoundingClientRect();
    Object.assign(box.style, { display: "block", left: r.left + "px", top: r.top + "px", width: r.width + "px", height: r.height + "px" });
  }

  const send = (msg) => framed && window.parent.postMessage(msg, origin);
  const target = (e) => (e.target && e.target.nodeType === 1 && e.target !== box ? e.target : null);

  window.addEventListener("mouseover", (e) => {
    if (!on) return;
    cur = target(e);
    show(cur);
  }, true);
  window.addEventListener("scroll", () => show(cur), true);
  window.addEventListener("resize", () => show(cur));
  // In pick mode the page's own handlers do not see the click.
  const swallow = (e) => {
    if (!on || !target(e)) return;
    e.preventDefault();
    e.stopPropagation();
    e.stopImmediatePropagation();
  };
  for (const t of ["pointerdown", "mousedown", "pointerup", "mouseup", "dblclick", "auxclick"]) window.addEventListener(t, swallow, true);
  window.addEventListener("click", (e) => {
    const el = target(e);
    if (!on || !el) return;
    swallow(e);
    send({
      type: "wb-pick",
      selector: selector(el),
      text: (el.innerText || el.textContent || "").trim().slice(0, 1000),
      html: htmlOf(el),
      rect: rectOf(el),
      url: location.pathname + location.search,
    });
  }, true);

  // With `watch`, the answer is sent again (same id) when the page scrolls,
  // is resized or changes, so the parent's labels follow the elements.
  let watching = null, again = 0;
  const resend = () => {
    if (!watching) return;
    clearTimeout(again);
    again = setTimeout(() => watching && who(watching), 250);
  };
  window.addEventListener("scroll", resend, true);
  window.addEventListener("resize", resend);
  const watchDom = () => {
    if (!document.body || !window.MutationObserver) return;
    new MutationObserver((ms) => { if (ms.some((m) => !box.contains(m.target) && m.target !== box)) resend(); })
      .observe(document.body, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["class", "style", "hidden"] });
  };
  if (document.body) watchDom(); else document.addEventListener("DOMContentLoaded", watchDom, { once: true });

  function who(m) {
    watching = m.watch ? m : null;
    const want = Array.isArray(m.texts) ? m.texts.filter((t) => typeof t === "string" && t.trim()).map((t) => t.trim()) : null;
    let root = document.body;
    try {
      if (m.selector) root = document.querySelector(m.selector);
    } catch (_) {
      root = null;
    }
    const max = Math.min(Number(m.max) || 300, 2000);
    const items = [];
    for (const el of root ? root.querySelectorAll("*") : []) {
      if (items.length >= max) break;
      if (el === box || /^(script|style|noscript|template|head|meta|link|title)$/.test(el.localName)) continue;
      let own = "";
      for (const n of el.childNodes) if (n.nodeType === 3) own += n.nodeValue;
      own = own.replace(/\s+/g, " ").trim();
      if (!own) continue;
      if (want && !want.some((t) => own.includes(t) || (own.length >= 3 && t.includes(own)))) continue;
      const rect = rectOf(el);
      if (!rect.width && !rect.height) continue;
      // Its classes and its parent's (a label's text often sits in a
      // span of the element an agent's diff names).
      const classes = [...new Set([...el.classList, ...(el.parentElement ? el.parentElement.classList : [])])].slice(0, 12);
      items.push({ selector: selector(el), text: own.slice(0, 200), classes, rect });
    }
    send({ type: "wb-who", id: m.id ?? null, items });
  }

  window.addEventListener("message", (e) => {
    if (e.source !== window.parent || e.origin !== origin) return;
    const m = e.data || {};
    if (m.type === "wb-pick-mode") {
      on = !!m.on;
      show(on ? cur : null);
    } else if (m.type === "wb-who") {
      who(m);
    }
  });
  send({ type: "wb-ready", url: location.pathname + location.search });

  // Runtime blame (SPEC.md, "Fourth build"): what the page throws, its
  // unhandled rejections and its console.error calls go to Workbench at
  // /preview/<agent>/__wb/runtime (the preview's cookie is the token there),
  // framed or not, at most 30 a minute. The page's own behaviour is kept:
  // console.error still prints, and nothing is caught or swallowed.
  const agent = (location.pathname.match(/^\/preview\/([^/]+)\//) || [])[1];
  if (agent) {
    const endpoint = `/preview/${agent}/__wb/runtime`;
    let start = Date.now(), n = 0, inside = false;
    const text = (v) => {
      if (v instanceof Error) return `${v.name}: ${v.message}`;
      if (typeof v === "string") return v;
      try { return JSON.stringify(v); } catch (_) { return String(v); }
    };
    const report = (r) => {
      const now = Date.now();
      if (now - start > 60000) { start = now; n = 0; }
      if (++n > 30 || inside) return;
      inside = true;
      try {
        const body = JSON.stringify({ ...r, message: String(r.message || "").slice(0, 2000), stack: String(r.stack || "").slice(0, 8000), url: location.pathname + location.search, ts_ms: now });
        const blob = new Blob([body], { type: "application/json" });
        if (!(navigator.sendBeacon && navigator.sendBeacon(endpoint, blob))) {
          fetch(endpoint, { method: "POST", body, headers: { "content-type": "application/json" }, credentials: "same-origin", keepalive: true }).catch(() => {});
        }
      } catch (_) {
        // Reporting never breaks the page.
      } finally {
        inside = false;
      }
    };
    window.addEventListener("error", (e) => {
      if (!(e instanceof ErrorEvent)) return; // a resource that did not load
      const err = e.error;
      report({ kind: "error", message: err ? text(err) : e.message, stack: (err && err.stack) || "", filename: e.filename || "", lineno: e.lineno || 0, colno: e.colno || 0 });
    });
    window.addEventListener("unhandledrejection", (e) => {
      const r = e.reason;
      report({ kind: "unhandledrejection", message: `Unhandled rejection: ${text(r)}`, stack: (r && r.stack) || "" });
    });
    const orig = console.error;
    console.error = function (...args) {
      const err = args.find((a) => a instanceof Error);
      report({ kind: "console", message: args.map(text).join(" "), stack: (err && err.stack) || (new Error().stack || "").split("\n").slice(2).join("\n") });
      return orig.apply(this, args);
    };
  }
})();
