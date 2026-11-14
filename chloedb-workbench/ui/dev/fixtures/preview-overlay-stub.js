// A stand-in for ui/preview-overlay.js (SPEC.md, "Preview"), served by
// levels-mock only while the real one is missing. It does what the parent
// page relies on:
//   - outlines what the pointer is on;
//   - a click posts {type: "wb-pick", selector, text, html (≤ 2 KB), rect}
//     to the parent and stops the page's own handler;
//   - {type: "wb-who", id?, watch?} from the parent is answered with
//     {type: "wb-who", id, elements: [{selector, text, classes, rect}]},
//     again on every scroll and resize while `watch` is on.
(() => {
  if (window.parent === window || window.__wbOverlay) return;
  window.__wbOverlay = true;
  const box = document.createElement("div");
  Object.assign(box.style, { position: "fixed", pointerEvents: "none", zIndex: 2147483647, border: "2px solid #f2b632", borderRadius: "6px", display: "none", boxShadow: "0 0 0 4px rgba(242,182,50,.18)" });
  document.documentElement.append(box);

  const rectOf = (el) => { const r = el.getBoundingClientRect(); return { x: r.left, y: r.top, width: r.width, height: r.height }; };
  function selectorOf(el) {
    const parts = [];
    for (let e = el; e && e.nodeType === 1 && e !== document.body && parts.length < 4; e = e.parentElement) {
      let s = e.tagName.toLowerCase();
      if (e.id) { parts.unshift(`${s}#${e.id}`); break; }
      const cls = [...e.classList].slice(0, 2);
      if (cls.length) s += "." + cls.join(".");
      const sib = e.parentElement ? [...e.parentElement.children].filter((x) => x.tagName === e.tagName) : [];
      if (sib.length > 1) s += `:nth-of-type(${sib.indexOf(e) + 1})`;
      parts.unshift(s);
    }
    return parts.join(" > ");
  }
  const ownText = (el) => [...el.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent).join(" ").replace(/\s+/g, " ").trim();
  const textOf = (el) => (ownText(el) || el.innerText || "").replace(/\s+/g, " ").trim().slice(0, 160);

  addEventListener("mousemove", (e) => {
    const el = e.target;
    if (!(el instanceof Element) || el === document.documentElement || el === document.body) { box.style.display = "none"; return; }
    const r = el.getBoundingClientRect();
    Object.assign(box.style, { display: "block", left: r.left - 2 + "px", top: r.top - 2 + "px", width: r.width + 4 + "px", height: r.height + 4 + "px" });
  }, true);
  addEventListener("mouseleave", () => { box.style.display = "none"; });
  addEventListener("click", (e) => {
    const el = e.target;
    if (!(el instanceof Element)) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    parent.postMessage({ type: "wb-pick", selector: selectorOf(el), text: textOf(el), html: el.outerHTML.slice(0, 2048), rect: rectOf(el) }, "*");
  }, true);

  let watching = null;
  function who(id) {
    const elements = [];
    for (const el of document.body.querySelectorAll("*")) {
      if (/^(SCRIPT|STYLE|BR)$/.test(el.tagName)) continue;
      const r = el.getBoundingClientRect();
      if (!r.width || !r.height) continue;
      const text = textOf(el);
      if (!el.classList.length && !ownText(el)) continue;
      elements.push({ selector: selectorOf(el), text, classes: [...el.classList], rect: rectOf(el) });
      if (elements.length >= 400) break;
    }
    parent.postMessage({ type: "wb-who", id, elements }, "*");
  }
  let raf = 0;
  const again = () => { if (watching == null || raf) return; raf = requestAnimationFrame(() => { raf = 0; who(watching); }); };
  addEventListener("scroll", again, true);
  addEventListener("resize", again);
  addEventListener("message", (e) => {
    if (e.source !== parent || !e.data || e.data.type !== "wb-who") return;
    watching = e.data.watch === false ? null : e.data.watch ? (e.data.id ?? 0) : watching;
    if (e.data.watch !== false) who(e.data.id ?? 0);
  });
})();
