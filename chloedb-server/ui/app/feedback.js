// Feedback on the command center: a screenshot of the view as it is, marked
// up (boxes, arrows, a pen, text, and black boxes over what must not be
// kept), with a note, sent to the server (POST /v1/feedback), which keeps it
// as a row of table cc_feedback. The Feedback view (views/feedback.js)
// reads and triages the rows; agents read them with SQL, the API or the
// MCP tools.
//
// The screenshot is made here, in the browser, with nothing sent anywhere
// else: the page is copied, its stylesheet put beside it, and the copy
// drawn as an SVG image onto a canvas ("From the page"). The screen-capture
// prompt of the browser ("From the screen") is the other way, pixel for
// pixel, for what a copy of the page does not show (an app's frame). What
// is sent is one picture with the marks drawn on it, so a part blacked out
// is not in what the server keeps; the marks go too, as numbers, places
// and the element under each, which is what an agent reads.

import { h, icon, clear, api, toast, can, needs, link, menu } from "./core.js";
import { state, clusterName, environment } from "./data.js";

// ---------------------------------------------------------------- errors seen on the page
// The last few, for a bug report (the reporter chooses to send them).
const ERRORS = [];
function keepError(msg) {
  const m = String(msg || "").replace(/\s+/g, " ").trim().slice(0, 400);
  if (!m) return;
  ERRORS.push({ at: Date.now(), msg: m });
  if (ERRORS.length > 12) ERRORS.shift();
}
addEventListener("error", (e) => keepError(`${e.message || "error"}${e.filename ? ` (${e.filename.split("/").pop()}:${e.lineno})` : ""}`));
addEventListener("unhandledrejection", (e) => keepError("unhandled: " + (e.reason?.message || e.reason)));
const consoleError = console.error.bind(console);
console.error = (...a) => { keepError(a.map((x) => (x instanceof Error ? x.message : typeof x === "string" ? x : (() => { try { return JSON.stringify(x); } catch { return String(x); } })())).join(" ")); consoleError(...a); };

// ---------------------------------------------------------------- the view this is about
let viewOf = () => ({ id: null, title: document.title });
/** For the shell: a function that says which view shows ({id, title}). */
export function setViewOf(f) { viewOf = f; }

// ---------------------------------------------------------------- the screenshot, from the page
const XHTML = "http://www.w3.org/1999/xhtml";
const mine = (el) => el.classList?.contains("fb-ui") || el.classList?.contains("tip");
// What shows a secret (a new key, a push key, a connector's secrets) is
// marked `data-secret`: the picture has dots in its place, a mark never
// names it, and a capture of the screen blacks it out.
const SECRET = "[data-secret]";
const isSecret = (el) => el.nodeType === 1 && el.matches(SECRET);
const isDark = () => document.documentElement.dataset.theme ? document.documentElement.dataset.theme === "dark" : matchMedia("(prefers-color-scheme: dark)").matches;

function styles() {
  let out = "";
  for (const sh of document.styleSheets) {
    try { for (const r of sh.cssRules) out += r.cssText + "\n"; } catch { /* another origin's: none here */ }
  }
  // A copy is drawn once: no animation (a drawer would be caught as it
  // slides in), no caret.
  return out + "\n*,*::before,*::after{animation:none!important;transition:none!important;caret-color:transparent!important}\n";
}

/** An image of what an element draws (a canvas, a loaded image), or null. */
function pictureOf(el) {
  try {
    if (el.localName === "canvas") return el.toDataURL("image/png");
    if (el.localName === "img" && el.complete && el.naturalWidth) {
      if (/^data:/.test(el.currentSrc || el.src)) return el.currentSrc || el.src;
      const c = document.createElement("canvas");
      c.width = el.naturalWidth; c.height = el.naturalHeight;
      c.getContext("2d").drawImage(el, 0, 0);
      return c.toDataURL("image/png");
    }
  } catch { /* another origin's image: left out */ }
  return null;
}

function stand(src, what) {
  const r = src.getBoundingClientRect();
  const d = document.createElement("div");
  d.setAttribute("style", `width:${r.width}px;height:${r.height}px;display:${getComputedStyle(src).display === "inline" ? "inline-block" : "block"};box-sizing:border-box;border:1px dashed var(--line-2);background:var(--sunken);color:var(--muted);font:12px var(--sans);display:grid;place-items:center`);
  d.textContent = what;
  return d;
}

/** A secret's place in the picture: its box, with dots and none of its words. */
function masked(src) {
  const r = src.getBoundingClientRect();
  const c = document.createElement(src.localName === "pre" || src.localName === "textarea" ? "div" : src.localName);
  for (const a of ["class", "style"]) if (src.hasAttribute(a)) c.setAttribute(a, src.getAttribute(a));
  c.style.width = r.width + "px"; c.style.height = r.height + "px"; c.style.boxSizing = "border-box"; c.style.overflow = "hidden";
  if (c.localName === "input") c.setAttribute("value", "••••••••");
  else c.textContent = "•••••••• a secret: not in the picture";
  return c;
}

/** A copy of `src` for the picture: values of fields, canvases and images as pictures, scrolled boxes as scrolled. */
function copyOf(src) {
  if (src.nodeType === 3) return document.createTextNode(src.nodeValue);
  if (src.nodeType !== 1 || mine(src)) return null;
  const tag = src.localName;
  if (["script", "noscript", "template", "link", "style"].includes(tag)) return null;
  if (isSecret(src)) return masked(src);
  if (tag === "iframe" || tag === "object" || tag === "embed") return stand(src, "An app's frame: use From the screen to take it");
  if (tag === "canvas" || tag === "video") {
    const p = tag === "canvas" ? pictureOf(src) : null;
    if (!p) return stand(src, tag === "video" ? "A video" : "A drawing");
    const r = src.getBoundingClientRect();
    const img = document.createElement("img");
    for (const a of ["class", "style"]) if (src.hasAttribute(a)) img.setAttribute(a, src.getAttribute(a));
    img.style.width = r.width + "px"; img.style.height = r.height + "px";
    img.setAttribute("src", p);
    return img;
  }
  const c = src.cloneNode(false);
  if (tag === "img") { const p = pictureOf(src); if (p) c.setAttribute("src", p); else c.removeAttribute("src"); c.removeAttribute("srcset"); }
  for (const k of src.childNodes) { const x = copyOf(k); if (x) c.append(x); }
  if (tag === "input") {
    if (src.type === "checkbox" || src.type === "radio") { if (src.checked) c.setAttribute("checked", ""); else c.removeAttribute("checked"); }
    else if (src.type === "password") c.setAttribute("value", src.value ? "••••••••" : "");
    else c.setAttribute("value", src.value);
  } else if (tag === "textarea") c.textContent = src.value;
  else if (tag === "select") { [...c.options].forEach((o, i) => { if (src.options[i]?.selected) o.setAttribute("selected", ""); else o.removeAttribute("selected"); }); }
  // A box scrolled: its children moved up (a sticky one stays where it sticks).
  if ((src.scrollTop || src.scrollLeft) && tag !== "html" && tag !== "body") {
    [...c.children].forEach((k, i) => {
      const s = src.children[i];
      if (s && getComputedStyle(s).position === "sticky") return;
      k.style.translate = `${-src.scrollLeft}px ${-src.scrollTop}px`;
    });
    c.style.overflow = "hidden";
  }
  if (src.open && tag === "details") c.setAttribute("open", "");
  return c;
}

/** The view as an image: draws a copy of the page onto a canvas. */
async function fromPage() {
  const W = innerWidth, H = innerHeight, scale = Math.min(Math.max(devicePixelRatio || 1, 1), 2);
  const root = document.documentElement;
  const html = document.createElementNS(XHTML, "html");
  for (const a of root.attributes) html.setAttribute(a.name, a.value);
  html.setAttribute("data-theme", isDark() ? "dark" : "light");
  const head = document.createElementNS(XHTML, "head");
  const st = document.createElementNS(XHTML, "style");
  st.textContent = styles();
  head.append(st);
  const body = copyOf(document.body);
  if (scrollX || scrollY) body.style.translate = `${-scrollX}px ${-scrollY}px`;
  html.append(head, body);
  const inner = new XMLSerializer().serializeToString(html);
  const theme = isDark() ? "dark" : "light";
  const style = (root.getAttribute("style") || "").replace(/"/g, "'");
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${W * scale}" height="${H * scale}" viewBox="0 0 ${W} ${H}" data-theme="${theme}" class="${root.className}" style="${style}"><foreignObject x="0" y="0" width="${W}" height="${H}">${inner}</foreignObject></svg>`;
  const img = new Image();
  img.decoding = "sync";
  img.src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(svg);
  await img.decode();
  const c = document.createElement("canvas");
  c.width = Math.round(W * scale); c.height = Math.round(H * scale);
  const g = c.getContext("2d");
  g.fillStyle = getComputedStyle(document.body).backgroundColor || "#fff";
  g.fillRect(0, 0, c.width, c.height);
  g.drawImage(img, 0, 0, c.width, c.height);
  // A canvas the copy tainted cannot be sent: say so now, not at the end.
  c.toDataURL("image/png").slice(0, 8);
  return { canvas: c, w: W, h: H, scale, how: "page", mapped: true };
}

/** The view as the browser's screen capture takes it (the browser asks first). */
async function fromScreen() {
  if (!navigator.mediaDevices?.getDisplayMedia) throw new Error("this browser has no screen capture");
  const stream = await navigator.mediaDevices.getDisplayMedia({ video: { displaySurface: "browser", frameRate: 5 }, audio: false, preferCurrentTab: true, selfBrowserSurface: "include", surfaceSwitching: "exclude" });
  try {
    const v = document.createElement("video");
    v.muted = true; v.playsInline = true; v.srcObject = stream;
    await v.play();
    // The prompt's own frames go first.
    await new Promise((r) => setTimeout(r, 350));
    const c = document.createElement("canvas");
    c.width = v.videoWidth; c.height = v.videoHeight;
    c.getContext("2d").drawImage(v, 0, 0);
    const scale = c.width / innerWidth;
    // The marks name what is under them only when this tab is what was taken.
    const mapped = Math.abs(c.height / scale - innerHeight) < 4;
    // A secret on the page is blacked out where it shows; when what was
    // taken is not this tab as it is, where it shows is not known, and
    // the picture is not kept.
    const secrets = [...document.querySelectorAll(SECRET)].map((e) => e.getBoundingClientRect()).filter((r) => r.width && r.height && r.bottom > 0 && r.right > 0 && r.top < innerHeight && r.left < innerWidth);
    if (secrets.length && !mapped) throw new Error("a secret (a new key) is on the page: close it, then take the picture");
    const g = c.getContext("2d");
    g.fillStyle = "#111";
    for (const r of secrets) g.fillRect(Math.floor((r.left - 2) * scale), Math.floor((r.top - 2) * scale), Math.ceil((r.width + 4) * scale), Math.ceil((r.height + 4) * scale));
    return { canvas: c, w: innerWidth, h: mapped ? innerHeight : c.height / scale, scale, how: "screen", mapped };
  } finally { stream.getTracks().forEach((t) => t.stop()); }
}

// ---------------------------------------------------------------- what is under a mark
function selectorOf(el) {
  const parts = [];
  for (let e = el; e && e.nodeType === 1 && e !== document.body && parts.length < 4; e = e.parentElement) {
    let p = e.localName;
    if (e.id) { parts.unshift(p + "#" + e.id); break; }
    const cls = [...e.classList].filter((c) => /^[a-z][\w-]*$/i.test(c) && c.length < 24).slice(0, 2);
    if (cls.length) p += "." + cls.join(".");
    if (e.dataset?.view) p += `[data-view="${e.dataset.view}"]`;
    parts.unshift(p);
  }
  return parts.join(" > ");
}
const tidy = (t, n = 120) => String(t || "").replace(/\s+/g, " ").trim().slice(0, n) || undefined;
const NAMED = "button,a,input,select,textarea,th,td,li,label,h1,h2,h3,h4,[role],.kpi,.pill,.chip,.badge";
// Elements whose own words are their label. Anything else (a cell, a row, a
// card, a field) is named by its label alone, never by what it holds: a mark
// says where it points, and what is there stays in the picture, where the
// sender can black it out.
const WORDED = "button,a,th,label,h1,h2,h3,h4,.pill,.chip,.badge,[role=tab],[role=button],[role=menuitem]";
const FIELD = "input,select,textarea";
const under = (x, y) => document.elementsFromPoint(x, y).find((e) => !e.closest(".fb-ui"));
const rectOf = (m) => ({ left: Math.min(m.x, m.x + m.w), top: Math.min(m.y, m.y + m.h), right: Math.max(m.x, m.x + m.w), bottom: Math.max(m.y, m.y + m.h) });
const meets = (a, b) => a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
/** What an element is called, from its own attributes and words, never a field's value. */
function labelOf(e) {
  if (e.closest(SECRET)) return "a secret";
  if (e.querySelector(SECRET)) return tidy(e.getAttribute("aria-label") || e.getAttribute("title"), 80);
  const own = e.getAttribute("aria-label") || e.getAttribute("title");
  if (own) return tidy(own, 80);
  if (e.matches(FIELD)) {
    const l = e.labels?.[0]?.textContent || e.getAttribute("placeholder") || e.getAttribute("name");
    return tidy(l ? `${l} (${e.type === "password" ? "a secret field" : e.localName})` : e.type === "password" ? "a secret field" : e.localName, 80);
  }
  if (e.matches("td")) {
    const th = e.closest("table")?.querySelectorAll("thead th")[e.cellIndex];
    return th ? tidy(`a ${th.textContent.trim()} cell`, 80) : "a table cell";
  }
  if (e.matches(WORDED) && !e.querySelector(FIELD)) return tidy(e.innerText || e.textContent, 80);
  return undefined;
}
/**
 * What is under a point, or (for a box) the smallest element that holds the
 * whole box. Nothing that meets a blacked-out area is named.
 */
function targetAt(x, y, box, hidden = []) {
  const el = under(x, y);
  if (!el || el === document.documentElement || el === document.body) return undefined;
  let hit = el.closest(NAMED) || el;
  if (box) {
    const holds = (e) => { const r = e.getBoundingClientRect(); return r.left <= box.x + 8 && r.top <= box.y + 8 && r.right >= box.x + box.w - 8 && r.bottom >= box.y + box.h - 8; };
    let e = el;
    while (e && e !== document.body && !holds(e)) e = e.parentElement;
    if (e && e !== document.body) hit = e;
  }
  const seen = (e) => { const r = e.getBoundingClientRect(); return !hidden.some((h) => meets(r, h)); };
  // Where it points, and no words, when what it points at is partly blacked out.
  if (!seen(hit)) return { selector: selectorOf(hit), role: hit.getAttribute("role") || hit.localName };
  const over = hit.closest(".drawer,.modal");
  const t = {
    selector: selectorOf(hit),
    text: labelOf(hit),
    card: tidy(hit.closest(".card")?.querySelector(".hd h3")?.textContent, 80),
    heading: tidy(over?.querySelector(".hd h2")?.textContent || document.querySelector("#content .page-head h1")?.textContent, 80),
    role: hit.getAttribute("role") || hit.localName,
  };
  // A box: the controls and headings inside it, by what they are called.
  if (box) {
    const inside = [...hit.querySelectorAll(NAMED)].filter((e) => { const r = e.getBoundingClientRect(); return r.width && r.left >= box.x - 2 && r.top >= box.y - 2 && r.right <= box.x + box.w + 2 && r.bottom <= box.y + box.h + 2 && seen(e); });
    const said = [...new Set(inside.map((e) => tidy(labelOf(e), 40)).filter(Boolean))].slice(0, 8);
    if (said.length) t.label = tidy(said.join(" · "), 160);
  }
  return t;
}
function pointOf(m) {
  if (m.tool === "box") return [m.x + m.w / 2, m.y + m.h / 2];
  if (m.tool === "arrow") return [m.x2, m.y2];
  if (m.tool === "pen") { const xs = m.points.map((p) => p[0]), ys = m.points.map((p) => p[1]); return [(Math.min(...xs) + Math.max(...xs)) / 2, (Math.min(...ys) + Math.max(...ys)) / 2]; }
  return [m.x, m.y];
}

// ---------------------------------------------------------------- drawing the marks
const COLORS = [["#e5484d", "Red"], ["#f5a524", "Amber"], ["#3e7bfa", "Blue"], ["#18a058", "Green"]];
const TOOLS = [["box", "Box", "B", "Draw a box around it"], ["arrow", "Arrow", "A", "Point at it"], ["pen", "Pen", "P", "Draw freely"], ["text", "Text", "T", "Write on the picture"], ["redact", "Black out", "X", "Hide what must not be kept (a key, a customer's data): it is not in what is sent"]];
const numbered = (m) => m.tool !== "redact";

function drawBadge(g, x, y, n, color) {
  g.save();
  g.beginPath(); g.arc(x, y, 10, 0, Math.PI * 2); g.fillStyle = color; g.fill();
  g.lineWidth = 2; g.strokeStyle = "#fff"; g.stroke();
  g.fillStyle = "#fff"; g.font = "700 11px system-ui, sans-serif"; g.textAlign = "center"; g.textBaseline = "middle";
  g.fillText(String(n), x, y + .5);
  g.restore();
}
function drawMark(g, m) {
  g.save();
  g.strokeStyle = m.color; g.fillStyle = m.color; g.lineWidth = 3; g.lineCap = "round"; g.lineJoin = "round";
  if (m.tool === "redact") { g.fillStyle = "#111"; g.fillRect(m.x, m.y, m.w, m.h); g.restore(); return; }
  if (m.tool === "box") {
    g.strokeRect(m.x, m.y, m.w, m.h);
    g.globalAlpha = .08; g.fillRect(m.x, m.y, m.w, m.h); g.globalAlpha = 1;
    drawBadge(g, m.x, m.y, m.n, m.color);
  } else if (m.tool === "arrow") {
    const a = Math.atan2(m.y2 - m.y, m.x2 - m.x), L = Math.min(18, Math.hypot(m.x2 - m.x, m.y2 - m.y) * .6);
    g.beginPath(); g.moveTo(m.x, m.y); g.lineTo(m.x2, m.y2); g.stroke();
    g.beginPath(); g.moveTo(m.x2, m.y2); g.lineTo(m.x2 - L * Math.cos(a - .45), m.y2 - L * Math.sin(a - .45)); g.lineTo(m.x2 - L * Math.cos(a + .45), m.y2 - L * Math.sin(a + .45)); g.closePath(); g.fill();
    drawBadge(g, m.x, m.y, m.n, m.color);
  } else if (m.tool === "pen") {
    g.beginPath(); m.points.forEach(([x, y], i) => (i ? g.lineTo(x, y) : g.moveTo(x, y))); g.stroke();
    drawBadge(g, m.points[0][0], m.points[0][1], m.n, m.color);
  } else if (m.tool === "text") {
    const t = m.text || "…";
    g.font = "600 14px system-ui, sans-serif";
    const lines = t.split("\n").slice(0, 8), w = Math.max(...lines.map((l) => g.measureText(l).width)) + 16, ht = lines.length * 18 + 8;
    g.beginPath(); g.roundRect ? g.roundRect(m.x, m.y, w, ht, 6) : g.rect(m.x, m.y, w, ht); g.fill();
    g.fillStyle = "#fff"; g.textBaseline = "top";
    lines.forEach((l, i) => g.fillText(l, m.x + 8, m.y + 5 + i * 18));
    drawBadge(g, m.x, m.y, m.n, m.color);
  }
  g.restore();
}

// ---------------------------------------------------------------- the editor
let open = false;

/** Takes the view as it is and opens the editor over it. */
export async function openFeedback(opts = {}) {
  if (open) return;
  open = true;
  const v = viewOf();
  let shot = null;
  try { shot = opts.how === "screen" ? await fromScreen() : opts.how === "none" ? null : await fromPage(); }
  catch (e) { console.warn("feedback: the page could not be taken", e); toast(`The page could not be taken (${e.message || e}). Try From the screen, or send a note alone.`, "bad", 6000); shot = null; }
  editor(shot, v);
}

function editor(shot, view) {
  let tool = "box", color = COLORS[0][0], marks = [], undone = [], drawing = null, kind = "bug", sending = false;
  const close = () => { open = false; removeEventListener("keydown", onKey, true); removeEventListener("resize", fit); root.remove(); };

  // The picture: the screenshot under, the marks over, both of the
  // screenshot's pixels; drawn in the page's CSS pixels (x scale).
  const base = h("canvas", { class: "fb-base" }), over = h("canvas", { class: "fb-over" });
  const stage = h("div", { class: "fb-stage" }), frame = h("div", { class: "fb-frame" }, base, over);
  stage.append(frame);
  const textIn = h("textarea", { class: "fb-textin hidden", rows: 2, placeholder: "Write, then Enter" });
  frame.append(textIn);
  function setShot(s) {
    shot = s;
    clear(stage, frame);
    if (!s) { stage.append(h("div", { class: "fb-noshot" }, icon("media"), h("b", {}, "No screenshot"), h("div", { class: "small dim" }, "This sends the note alone. Take the view again from the bar above to add one."))); frame.remove(); redrawList(); return; }
    stage.append(frame);
    base.width = over.width = s.canvas.width; base.height = over.height = s.canvas.height;
    base.getContext("2d").drawImage(s.canvas, 0, 0);
    marks = []; undone = [];
    fit(); paint(); redrawList();
  }
  function fit() {
    if (!shot) return;
    const r = stage.getBoundingClientRect();
    const k = Math.min((r.width - 24) / shot.w, (r.height - 24) / shot.h, 1.5);
    frame.style.width = shot.w * k + "px"; frame.style.height = shot.h * k + "px";
  }
  function paint(extra) {
    const g = over.getContext("2d");
    g.setTransform(1, 0, 0, 1, 0, 0); g.clearRect(0, 0, over.width, over.height);
    g.setTransform(shot.scale, 0, 0, shot.scale, 0, 0);
    for (const m of marks) drawMark(g, m);
    if (extra) drawMark(g, extra);
  }
  const renumber = () => { let n = 0; for (const m of marks) m.n = numbered(m) ? ++n : 0; };
  const nextN = () => marks.filter(numbered).length + 1;
  function at(e) { const r = over.getBoundingClientRect(); return [Math.max(0, Math.min(shot.w, (e.clientX - r.left) / r.width * shot.w)), Math.max(0, Math.min(shot.h, (e.clientY - r.top) / r.height * shot.h))]; }
  function add(m) { marks.push(m); undone = []; renumber(); paint(); redrawList(); }

  over.addEventListener("pointerdown", (e) => {
    if (!shot || e.button > 0) return;
    e.preventDefault();
    const [x, y] = at(e);
    if (tool === "text") { placeText(x, y); return; }
    over.setPointerCapture(e.pointerId);
    drawing = tool === "pen" ? { tool, color, n: nextN(), points: [[x, y]] } : tool === "arrow" ? { tool, color, n: nextN(), x, y, x2: x, y2: y } : { tool, color, n: tool === "redact" ? 0 : nextN(), x0: x, y0: y, x, y, w: 0, h: 0 };
  });
  over.addEventListener("pointermove", (e) => {
    if (!drawing) return;
    const [x, y] = at(e);
    if (drawing.tool === "pen") drawing.points.push([Math.round(x * 10) / 10, Math.round(y * 10) / 10]);
    else if (drawing.tool === "arrow") { drawing.x2 = x; drawing.y2 = y; }
    else { drawing.x = Math.min(x, drawing.x0); drawing.y = Math.min(y, drawing.y0); drawing.w = Math.abs(x - drawing.x0); drawing.h = Math.abs(y - drawing.y0); }
    paint(drawing);
  });
  const finish = () => {
    if (!drawing) return;
    const d = drawing; drawing = null;
    const big = d.tool === "pen" ? d.points.length > 2 : d.tool === "arrow" ? Math.hypot(d.x2 - d.x, d.y2 - d.y) > 8 : d.w > 5 && d.h > 5;
    if (!big) { paint(); return; }
    delete d.x0; delete d.y0;
    add(d);
    // The new mark's line, to say what it is about.
    setTimeout(() => list.querySelector(`[data-n="${d.n}"] input`)?.focus(), 0);
  };
  over.addEventListener("pointerup", finish);
  over.addEventListener("pointercancel", finish);

  function placeText(x, y) {
    const r = over.getBoundingClientRect(), k = r.width / shot.w;
    textIn.classList.remove("hidden");
    textIn.style.left = x * k + "px"; textIn.style.top = y * k + "px";
    textIn.value = "";
    textIn.focus();
    textIn.onkeydown = (e) => {
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); const t = textIn.value.trim(); textIn.classList.add("hidden"); if (t) add({ tool: "text", color, n: nextN(), x, y, text: t }); }
      else if (e.key === "Escape") { e.stopPropagation(); textIn.classList.add("hidden"); }
    };
    textIn.onblur = () => textIn.classList.add("hidden");
  }

  function undo() { const m = marks.pop(); if (m) { undone.push(m); renumber(); paint(); redrawList(); } }
  function redo() { const m = undone.pop(); if (m) { marks.push(m); renumber(); paint(); redrawList(); } }

  // ---- the bar
  const toolBtns = TOOLS.map(([id, label, key, title]) => h("button", { type: "button", class: "btn sm fb-tool", dataset: { tool: id }, title: `${title} (${key})`, "aria-pressed": String(id === tool), onclick: () => setTool(id) }, icon({ box: "fbbox", arrow: "fbarrow", pen: "edit", text: "fbtext", redact: "fbredact" }[id]), h("span", { class: "lbl" }, label)));
  function setTool(t) { tool = t; for (const b of toolBtns) b.setAttribute("aria-pressed", String(b.dataset.tool === t)); over.dataset.tool = t; }
  const colorBtns = COLORS.map(([c, name]) => h("button", { type: "button", class: "fb-color", title: name, "aria-label": name, "aria-pressed": String(c === color), style: { "--c": c }, onclick: () => { color = c; for (const b of colorBtns) b.setAttribute("aria-pressed", String(b.style.getPropertyValue("--c") === c)); } }));
  const retake = async (how) => {
    root.classList.add("fb-away");
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    try { setShot(how === "none" ? null : how === "screen" ? await fromScreen() : await fromPage()); }
    catch (e) { if (e?.name !== "NotAllowedError") toast(`Not taken: ${e.message || e}`, "bad"); }
    root.classList.remove("fb-away");
  };
  const bar = h("header", { class: "fb-bar" },
    h("div", { class: "fb-title" }, icon("feedback"), h("b", {}, "Feedback"), h("span", { class: "dim small" }, view.title ? "on " + view.title : "")),
    h("div", { class: "fb-tools", role: "group", "aria-label": "Marks" }, toolBtns),
    h("div", { class: "fb-colors", role: "group", "aria-label": "Color" }, colorBtns),
    h("button", { type: "button", class: "btn ghost icon", title: "Undo (Ctrl Z)", onclick: undo }, icon("undo")),
    h("button", { type: "button", class: "btn ghost icon", title: "Redo (Ctrl Shift Z)", onclick: redo }, icon("redo")),
    h("div", { class: "grow" }),
    h("button", { type: "button", class: "btn sm fb-retake", title: "Take the picture again: from the page, from the screen, or none", "aria-haspopup": "menu", onclick: (e) => menu(e.currentTarget, [
      { label: "From the page (a copy, no prompt)", icon: "camera", onclick: () => retake("page") },
      navigator.mediaDevices?.getDisplayMedia ? { label: "From the screen (the browser asks)", icon: "media", onclick: () => retake("screen") } : null,
      { label: "No picture: the note alone", icon: "x", onclick: () => retake("none") },
    ].filter(Boolean)) }, icon("camera"), h("span", { class: "lbl" }, "Take again")),
    h("button", { type: "button", class: "btn ghost icon", title: "Close without sending (Esc)", "aria-label": "Close", onclick: () => close() }, icon("x")));

  // ---- the side: what it is, the note, the marks, what goes with it
  const kinds = [["bug", "Bug", "Something is wrong"], ["change", "Change", "Make it work or look another way"], ["idea", "Idea", "Something new"], ["question", "Question", "Something is not clear"]];
  const kindBox = h("div", { class: "seg fb-kind", role: "group", "aria-label": "Kind" }, kinds.map(([k, l, t]) => h("button", { type: "button", title: t, dataset: { v: k }, "aria-pressed": String(k === kind), onclick: (e) => { kind = k; for (const b of kindBox.children) b.setAttribute("aria-pressed", String(b.dataset.v === k)); } }, l)));
  const note = h("textarea", { class: "input fb-note", rows: 5, placeholder: "What is wrong, or what you want instead. The numbers of your marks help: \"2 should say when it started\".", "aria-label": "Note" });
  const list = h("ol", { class: "fb-marks" });
  const errs = ERRORS.slice();
  const withErrors = h("input", { type: "checkbox", checked: errs.length > 0 });
  const sendBtn = h("button", { type: "button", class: "btn primary", onclick: () => send() }, icon("check"), "Send");
  const writer = can("write", "POST /v1/feedback");
  if (!writer) { sendBtn.disabled = true; sendBtn.title = "Sending feedback " + needs("write", "POST /v1/feedback"); }
  function redrawList() {
    clear(list, marks.length ? marks.map((m) => h("li", { dataset: { n: String(m.n) }, class: "fb-mark" },
      m.tool === "redact" ? h("span", { class: "fb-n fb-n-redact", title: "Blacked out: not in the picture sent" }, "■") : h("span", { class: "fb-n", style: { background: m.color } }, String(m.n)),
      m.tool === "redact" ? h("span", { class: "small dim grow" }, "Blacked out") : h("input", { class: "input sm grow", value: m.text || "", placeholder: m.tool === "text" ? "Text" : `What about ${m.tool === "arrow" ? "this" : "this " + (m.tool === "box" ? "box" : "part")}? (optional)`, oninput: (e) => { m.text = e.target.value; if (m.tool === "text") paint(); } }),
      h("button", { type: "button", class: "btn ghost icon sm", title: "Remove this mark", "aria-label": "Remove", onclick: () => { marks = marks.filter((x) => x !== m); renumber(); paint(); redrawList(); } }, icon("x"))))
      : h("li", { class: "small dim fb-hint" }, shot ? "Draw on the picture: a box or an arrow at what you mean. Each gets a number to name in the note." : "No picture: the note alone is sent."));
  }
  const ctxLine = h("div", { class: "small dim" }, `Goes with it: ${view.title || "the view"} (${location.hash || "#/home"}), the window's size and theme, the server's version, and your key's name.`);
  const side = h("aside", { class: "fb-side" },
    h("label", { class: "fb-l" }, "What is it?"), kindBox,
    h("label", { class: "fb-l" }, "Note"), note,
    h("label", { class: "fb-l" }, "Marks"), list,
    ctxLine,
    errs.length ? h("label", { class: "row nw small", style: { gap: "6px" } }, withErrors, `Also the last ${errs.length === 1 ? "error" : errs.length + " errors"} of this page`, h("span", { class: "dim", title: errs.map((e) => e.msg).join("\n") }, " (?)")) : null,
    !writer ? h("div", { class: "fb-warn small" }, icon("lock"), "Sending feedback needs a write key. Sign in with one from the key menu.") : null,
    h("div", { class: "fb-send" }, h("button", { type: "button", class: "btn", onclick: () => close() }, "Cancel"), sendBtn));

  const root = h("div", { class: "fb-ui fb-ed", role: "dialog", "aria-modal": "true", "aria-label": "Send feedback" }, bar, h("div", { class: "fb-main" }, stage, side));
  document.body.append(root);
  setTool(tool);
  setShot(shot);
  addEventListener("resize", fit);
  setTimeout(() => (shot ? over : note).focus?.(), 0);
  if (!shot) note.focus();

  function onKey(e) {
    if (!root.isConnected) return;
    const typing = e.target.closest?.("input,textarea,select");
    if (e.key === "Escape") {
      if (e.target === textIn || document.querySelector(".menu")) return;
      if (typing && e.target !== note) { e.target.blur(); e.preventDefault(); return; }
      e.preventDefault(); e.stopPropagation(); if (!sending) close(); return;
    }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "z" && !typing) { e.preventDefault(); e.stopPropagation(); if (e.shiftKey) redo(); else undo(); return; }
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); send(); return; }
    // The command center's own shortcuts wait while this is open (main.js
    // skips them under .fb-ed); a field gets its keys.
    if (typing || e.ctrlKey || e.metaKey || e.altKey) return;
    const t = TOOLS.find((x) => x[2].toLowerCase() === e.key.toLowerCase());
    if (t) { setTool(t[0]); e.preventDefault(); }
  }
  addEventListener("keydown", onKey, true);

  // ---- sending
  function flat() {
    const c = document.createElement("canvas");
    c.width = base.width; c.height = base.height;
    const g = c.getContext("2d");
    g.drawImage(base, 0, 0);
    g.setTransform(shot.scale, 0, 0, shot.scale, 0, 0);
    for (const m of marks) drawMark(g, m);
    return c;
  }
  function encode(c) {
    const LIMIT = 4.5e6; // characters of the data: URL; the server takes 6 MB of picture
    let u = c.toDataURL("image/png");
    if (u.length > LIMIT) { const w = c.toDataURL("image/webp", .9); if (w.startsWith("data:image/webp")) u = w; }
    if (u.length > LIMIT) u = c.toDataURL("image/jpeg", .88);
    if (u.length > LIMIT) {
      const s = document.createElement("canvas");
      s.width = Math.round(c.width / 2); s.height = Math.round(c.height / 2);
      s.getContext("2d").drawImage(c, 0, 0, s.width, s.height);
      u = s.toDataURL("image/jpeg", .85);
    }
    return u;
  }
  function thumbOf(c) {
    const w = 480, s = document.createElement("canvas");
    s.width = w; s.height = Math.max(1, Math.round(c.height * w / c.width));
    const g = s.getContext("2d");
    g.imageSmoothingQuality = "high";
    g.drawImage(c, 0, 0, s.width, s.height);
    return s.toDataURL("image/jpeg", .78);
  }
  async function send() {
    if (sending || !writer) return;
    const text = note.value.trim();
    if (!text && !marks.some((m) => (m.text || "").trim())) { toast("Say what is wrong or what you want: in the note, or on a mark", "bad"); note.focus(); return; }
    sending = true; sendBtn.disabled = true; sendBtn.lastChild.textContent = "Sending…";
    try {
      const hidden = marks.filter((m) => m.tool === "redact").map(rectOf);
      const out = marks.map((m) => {
        const o = { n: m.n, tool: m.tool, color: m.color };
        for (const k of ["x", "y", "w", "h", "x2", "y2"]) if (m[k] != null) o[k] = Math.round(m[k] * 10) / 10;
        if (m.points) o.points = m.points;
        if ((m.text || "").trim()) o.text = m.text.trim();
        if (m.tool !== "redact" && shot?.mapped) { const [x, y] = pointOf(m); o.target = targetAt(x, y, m.tool === "box" ? m : null, hidden); }
        return o;
      });
      const body = {
        kind, note: text || null, marks: out,
        page: view.id || null, route: location.hash || "#/home",
        url: location.origin + location.pathname + location.hash,
        context: {
          title: view.title || null,
          viewport: { w: innerWidth, h: innerHeight, dpr: devicePixelRatio || 1 },
          theme: isDark() ? "dark" : "light", narrow: innerWidth < 900,
          user_agent: navigator.userAgent, language: navigator.language,
          cluster: clusterName(), environment: environment(),
          version: state.system?.version || null, backend: state.system?.backend || null,
          key: state.whoami ? { name: state.whoami.name, scope: state.whoami.scope } : null,
          paused: !!state.paused,
          capture: shot ? shot.how : "none",
          errors: withErrors.checked ? errs.map((e) => ({ at: new Date(e.at).toISOString(), msg: e.msg })) : [],
        },
      };
      if (shot) { const c = flat(); body.shot = encode(c); body.thumb = thumbOf(c); body.shot_w = c.width; body.shot_h = c.height; }
      const r = await api("/v1/feedback", { body, timeout: 60000 });
      close();
      const t = h("span", {}, `Feedback sent (${r.id}). `, h("a", { href: link("feedback", { id: r.id }) }, "See it"));
      toast(t, "ok", 6000);
      dispatchEvent(new CustomEvent("chloedb:feedback", { detail: r }));
    } catch (e) {
      sending = false; sendBtn.disabled = false; sendBtn.lastChild.textContent = "Send";
      toast(e.status === 403 ? `Not sent: ${e.message}` : `Not sent: ${e.message}`, "bad", 7000);
    }
  }
}
