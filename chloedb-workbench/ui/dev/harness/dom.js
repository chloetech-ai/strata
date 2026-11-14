// The harness mock's pieces, written as HTML strings in the markup of the
// style mock (C-instrument.html): the toolbar, the frame of content and
// inspector, the phone's tab bar, labels, key caps, tick scales, gauges and
// tables. The router (app.js) puts a page's {crumb, content, insp, mdock}
// into the frame. The router pattern and the screenshot script are copied
// from the journeys mock (ui/dev/journeys/app.js, shoot.cjs).

/** Escapes text for HTML. */
export const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

/** An icon from the sprite in harness.html. */
export const ic = (name, style = "") => `<svg class="i"${style ? ` style="${style}"` : ""}><use href="#${name}"/></svg>`;

/** A key cap. */
export const kc = (k) => `<span class="kc">${esc(k)}</span>`;

/** A small-caps mono label, with an optional count and a link at the right. */
export function lbl(text, { n, link, href = "#", right, strong = true, style = "" } = {}) {
  return `<div class="lbl"${style ? ` style="${style}"` : ""}><span class="${strong ? "c" : ""}">${text}</span>${n != null ? `<span class="num">${n}</span>` : ""}<span class="sp"></span>${right || ""}${link ? `<a href="${href}">${link}</a>` : ""}</div>`;
}

export function btn(label, { pri, ghost, k, href, act, cls = "", style = "" } = {}) {
  const c = `btn${pri ? " pri" : ""}${ghost ? " ghost" : ""}${cls ? " " + cls : ""}`;
  const inner = `${label}${k ? kc(k) : ""}`;
  return href ? `<a class="${c}" href="${href}"${style ? ` style="${style}"` : ""}>${inner}</a>`
    : `<button class="${c}" type="button"${act ? ` data-act="${act}"` : ""}${style ? ` style="${style}"` : ""}>${inner}</button>`;
}

export const chip = (t, cls = "") => `<span class="chip${cls ? " " + cls : ""}">${t}</span>`;

/** A scale of `total` ticks with `on` of them lit (the style's gauges). */
export function ticks(on, total = 24, { tone = "", mark } = {}) {
  let s = "";
  for (let i = 0; i < total; i++) s += `<i class="${i < on ? "on" : ""}${mark === i ? " mk" : ""}"></i>`;
  return `<div class="ticks${tone ? " " + tone : ""}">${s}</div>`;
}

/** A gauge tile: a label, a mono figure, its unit, a scale, a delta line. */
export function gauge({ label, v, unit, on, total = 24, tone, delta, dtone, names }) {
  return `<div class="gauge"><div class="lbl">${label}</div>
    <div class="v"><b class="${tone || ""}">${v}</b>${unit ? `<span>${unit}</span>` : ""}</div>
    ${on != null ? ticks(on, total, { tone }) : ""}
    ${delta ? `<div class="dl ${dtone || ""}">${delta}</div>` : ""}
    ${names ? `<div class="names">${names}</div>` : ""}</div>`;
}

/** A state dot in words (st need / work / ready). */
export const st = (kind, text) => `<span class="st ${kind}"><i></i>${text}</span>`;

/** An evidence level: counted (exact, from rows), recomputed (exact, ChloeCtx's receipts on their snapshots), replayed (sessions run again, N times). */
export const lv = (k) => `<span class="lv ${k}" title="${{ counted: "Exact: counted from recorded rows, no model", recomputed: "Exact: ChloeCtx's recall run again on the snapshot of each receipt", replayed: "Measured: sessions run again N times in a sandbox", judged: "A model's judgement, labelled as such" }[k]}">${k}</span>`;

/** Run cells: p pass, f fail, r running, q queued, s skipped. */
export const cells = (s) => `<span class="cells">${s.split("").map((c) => `<i class="${c}"></i>`).join("")}</span>`;

/** The harness's own row of places, under the title (like Review's tabs). */
export function htabs(on) {
  const t = [["overview", "Overview", "#/harness"], ["optimize", "Optimize", "#/harness/run/7"], ["edit", "Editor", "#/harness/edit/permissions"], ["sees", "What the model sees", "#/harness/sees"], ["evals", "Evals", "#/harness/evals"], ["versions", "Versions", "#/harness/versions"]];
  return `<nav class="tabs-u">${t.map(([k, l, h]) => `<a class="${k === on ? "on" : ""}" href="${h}">${l}</a>`).join("")}</nav>`;
}

/** The kicker, title and sentence every harness page opens with. */
export function head({ kicker, title, sub, chips, tabs }) {
  return `${kicker ? `<div class="lbl">${kicker}</div>` : ""}
    <h1 class="title" style="margin-top:6px">${title}</h1>
    ${sub ? `<p class="sub">${sub}</p>` : ""}
    ${chips ? `<div class="chips">${chips.join("")}</div>` : ""}
    ${tabs ? htabs(tabs) : ""}`;
}

/** The toolbar, as in the style mock: mark, breadcrumb (mono, slashes), Search, Needs you, agents. */
export function toolbar(crumb) {
  const parts = crumb.split("|");
  const c = parts.map((p, i) => i === parts.length - 1 ? `<b>${p}</b>` : `<span>${p}</span><span class="sl">/</span>`).join("");
  return `<header class="top">
    <a class="mark" href="#/"><i></i>Workbench</a>
    <nav class="crumb">${c}</nav>
    <span class="sp"></span>
    <div class="search">${ic("why")}Search shop<span class="kc">/</span></div>
    <a class="tb need" href="#/"><span class="n">2</span>need you</a>
    <span class="tb agents"><span class="dots"><i class="a"></i><i></i><i></i><i></i><i></i></span><span class="num">5</span> agents</span>
  </header>`;
}

/** The phone's tab bar, as in the style mock. */
export function tabbar(on) {
  const t = [["home", "Home", "home", "#/"], ["review", "Review", "merge", "#/"], ["why", "Why", "why", "#/"], ["needs", "Needs you", "inbox", "#/"], ["agents", "Agents", "agents", "#/"]];
  return `<nav class="tabbar">${t.map(([k, l, i, h]) => `<a class="${k === on ? "on" : ""}" href="${h}">${ic(i)}${l}${k === "needs" ? '<span class="badge">2</span>' : ""}</a>`).join("")}</nav>`;
}
