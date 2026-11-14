// Map (SPEC.md, "Levels", Map): the repository as territory. A squarified
// treemap of `GET /api/map`'s tree sized by lines, tinted by the agent
// changing each file, collisions hatched, ChloeCtx's ◆ decisions pinned on
// the files they name. A click on a folder drills in, the breadcrumb goes
// back out; a click on a pin opens its Why card.
import {
  h, useStyle, agentColor, agentTint, avatar, agentChip, ctxChip, pill, empty, fmtAgo, tooltip, onEscape,
  repoOf, throttle, card, setFleet,
} from "./common.js";

/** Squarified treemap (Bruls, Huizing, van Wijk): items {item, value} into r. */
export function squarify(items, r) {
  const out = [];
  const total = items.reduce((s, i) => s + i.value, 0);
  if (!total || r.w <= 0 || r.h <= 0) return out;
  const scale = (r.w * r.h) / total;
  let rest = items.map((i) => ({ item: i.item, area: i.value * scale })).sort((a, b) => b.area - a.area);
  let { x, y, w, h } = r;
  while (rest.length) {
    const side = Math.min(w, h);
    const worst = (row) => {
      const s = row.reduce((a, b) => a + b.area, 0);
      let mx = 0, mn = Infinity;
      for (const it of row) { mx = Math.max(mx, it.area); mn = Math.min(mn, it.area); }
      return Math.max((side * side * mx) / (s * s), (s * s) / (side * side * mn));
    };
    const row = [rest[0]];
    let i = 1;
    while (i < rest.length && worst([...row, rest[i]]) <= worst(row)) row.push(rest[i++]);
    rest = rest.slice(i);
    const sum = row.reduce((a, b) => a + b.area, 0);
    if (w >= h) {
      const cw = rest.length ? sum / h : w;
      let cy = y;
      for (const it of row) { const ih = (it.area / sum) * h; out.push({ item: it.item, x, y: cy, w: cw, h: ih }); cy += ih; }
      x += cw; w -= cw;
    } else {
      const rh = rest.length ? sum / w : h;
      let cx = x;
      for (const it of row) { const iw = (it.area / sum) * w; out.push({ item: it.item, x: cx, y, w: iw, h: rh }); cx += iw; }
      y += rh; h -= rh;
    }
  }
  return out;
}

const base = (p) => p.split("/").pop();
const parentOf = (p) => (p.includes("/") ? p.slice(0, p.lastIndexOf("/")) : "");
const under = (p, dir) => !dir || p === dir || p.startsWith(dir + "/");
const nameOf = (a) => (a && (a.name || a.id)) || "?";

// Time travel: the scrubber above the screen (ui/levels/timebar.js).
import { timeTravel } from "./timebar.js";
// Hotspots: the heat layer and its "Heat" toggle (ui/levels/hotspots.js).
import { withHeat } from "./hotspots.js";
// Sixth build: a repository read without an agent (ui/levels/repos.js).
import { repoPicker, repoSwitch, needsRepo } from "./repos.js";
export const mount = timeTravel(withHeat(mountMap), { screen: "map" });
/** The map without the time bar, its own title or a repository switch
 *  (JOURNEYS.md, Understand): `ctx.params.embed` set. */
export const mountBare = (root, ctx) => withHeat(mountMap)(root, { ...ctx, params: { ...(ctx.params || {}), embed: true } });
function mountMap(root, ctx) {
  const S = { data: null, err: null, path: "", sel: null, focus: null, sent: new Map(), busy: new Set(), needRepo: false, unfold: new Set() };
  const I = { nodes: new Map(), changed: new Map(), read: new Map(), coll: new Map(), pins: new Map(), agents: new Map(), agg: new Map() };
  const tip = tooltip();
  const el = h("div", { class: "lv-screen mp" });
  root.append(el);
  let tm = null, ro = null, frame = 0, dropEsc = null, dropUp = null, alive = true;

  async function load() {
    const repo = repoOf(ctx);
    // No repository yet: the picker, not an error (sixth build).
    if (!repo) { S.needRepo = true; if (alive) render(); return; }
    try {
      S.data = await ctx.api(`/api/map?repo=${encodeURIComponent(repo)}`);
      S.err = null; S.needRepo = false;
      index();
    } catch (e) { S.err = e.message || String(e); S.needRepo = needsRepo(e); }
    if (alive) render();
  }
  const refresh = throttle(load, 10000);

  function index() {
    for (const m of Object.values(I)) m.clear();
    const d = S.data;
    for (const a of ctx.state?.agents || []) I.agents.set(a.id, a);
    setFleet([...new Map([...(ctx.state?.agents || []), ...(d.agents || []).map((r) => r.agent)].map((a) => [a.id, a])).values()]);
    for (const row of d.agents || []) {
      I.agents.set(row.agent.id, row.agent);
      for (const f of row.files || []) {
        if (!I.changed.has(f.path)) I.changed.set(f.path, []);
        I.changed.get(f.path).push({ agent: row.agent, added: f.added || 0, removed: f.removed || 0 });
      }
      for (const p of row.reading || []) {
        if (!I.read.has(p)) I.read.set(p, []);
        I.read.get(p).push(row.agent);
      }
    }
    for (const c of d.collisions || []) I.coll.set(c.path, c);
    for (const p of d.pins || []) { if (!I.pins.has(p.path)) I.pins.set(p.path, []); I.pins.get(p.path).push(p); }
    // A fold the person opened shows its files (sixth build).
    const unfold = (n) => {
      if (!n.children) return;
      if (S.unfold.has(n.path)) n.children = n.children.flatMap((c) => (c.kind === "more" && Array.isArray(c.items) && c.items.length ? c.items : [c]));
      n.children.forEach(unfold);
    };
    unfold(d.tree);
    const walk = (n) => {
      I.nodes.set(n.path, n);
      const agg = { by: new Map(), coll: [], pins: 0, files: 0, changedFiles: [] };
      if (n.collapsed) agg.files = n.files || 0; // a deep folder, sent without its children
      if (n.children) {
        for (const c of n.children) {
          const g = walk(c);
          for (const [k, v] of g.by) agg.by.set(k, (agg.by.get(k) || 0) + v);
          agg.coll.push(...g.coll); agg.pins += g.pins; agg.files += g.files;
          agg.changedFiles.push(...g.changedFiles);
        }
      } else {
        agg.files = n.kind === "more" ? n.files || 1 : n.folded || 1;
        for (const c of I.changed.get(n.path) || []) agg.by.set(c.agent.id, (agg.by.get(c.agent.id) || 0) + c.added + c.removed);
        if (I.changed.has(n.path)) agg.changedFiles.push(n.path);
        if (I.coll.has(n.path)) agg.coll.push(I.coll.get(n.path));
        agg.pins = (I.pins.get(n.path) || []).length;
      }
      I.agg.set(n.path, agg);
      return agg;
    };
    walk(d.tree);
    if (!I.nodes.has(S.path)) S.path = "";
  }

  // ------------------------------------------------------------ rendering
  function render() {
    tip.hide();
    if (S.needRepo) {
      if (el.querySelector(".rp")) return; // the picker stays, with what was typed in it
      el.replaceChildren(head(), repoPicker(ctx, { why: "The map is a repository's tracked files sized by lines, with each agent's changes and the decisions ChloeCtx pinned to them. It needs no agent: open a git checkout." }));
      return;
    }
    if (!S.data) {
      el.replaceChildren(head(), S.err ? empty(`The map could not load: ${S.err}`) : empty("Reading the territory…"));
      return;
    }
    // Esc: the open card first (registered later, so on top), then up a folder.
    dropUp?.(); dropUp = S.path ? onEscape(() => drill(parentOf(S.path))) : null;
    tm = h("div", { class: "mp-tm", role: "group", "aria-label": "Treemap of the repository" });
    tm.addEventListener("mousemove", hover);
    tm.addEventListener("mouseleave", () => tip.hide());
    tm.addEventListener("focusin", (e) => { const t = e.target.closest("[data-path]"); if (t) showTip(t, null); });
    tm.addEventListener("focusout", () => tip.hide());
    el.replaceChildren(
      head(),
      legend(),
      h("div", { class: "mp-body" },
        h("div", { class: "mp-left" }, crumbs(), tm),
        h("aside", { class: "mp-side" }, collisionsCard(), selectionCard(), pinsCard())),
    );
    ro?.disconnect();
    ro = new ResizeObserver(() => { cancelAnimationFrame(frame); frame = requestAnimationFrame(drawTree); });
    ro.observe(tm);
  }

  function head() {
    if (ctx.params && ctx.params.embed) return h("header", { class: "lv-head" }, h("span", { class: "lv-sub" }, "Who is changing which file · size = lines · ", h("span", { class: "mp-dia", "aria-hidden": "true" }), " = a decision ChloeCtx can explain"));
    return h("header", { class: "lv-head" },
      h("h1", { class: "lv-h1" }, "Territory"),
      h("span", { class: "lv-sub" }, "who is working where · size = lines · ", h("span", { class: "mp-dia", "aria-hidden": "true" }), " = a decision ChloeCtx can explain"),
      repoOf(ctx) && !ctx.past ? repoSwitch(ctx, (S.data && S.data.repo) || repoOf(ctx)) : null);
  }

  function legend() {
    const rows = (S.data.agents || []).filter((r) => (r.files || []).length || (r.reading || []).length);
    return h("div", { class: "mp-legend", role: "toolbar", "aria-label": "Agents" },
      rows.map((r) => {
        const add = r.files.reduce((s, f) => s + (f.added || 0), 0), del = r.files.reduce((s, f) => s + (f.removed || 0), 0);
        const on = S.focus === r.agent.id;
        return h("button", {
          class: "mp-key" + (on ? " on" : ""), "aria-pressed": on ? "true" : "false", style: `--c:${agentColor(r.agent)}`,
          title: `${nameOf(r.agent)}: ${r.files.length} files changed, ${r.reading.length} read lately. Click to pick it out.`,
          onclick: () => { S.focus = on ? null : r.agent.id; render(); },
        }, h("i"), h("span", { class: "mp-key-n" }, nameOf(r.agent)),
        h("span", { class: "mp-key-d" }, r.files.length ? `+${add} −${del}` : "reading"));
      }),
      I.pins.size ? h("span", { class: "mp-key-pin" }, h("span", { class: "mp-dia" }), `${new Set([...I.pins.values()].flat().map((p) => p.record.id || p.record.text)).size} decisions`) : null,
    );
  }

  function crumbs() {
    const parts = S.path ? S.path.split("/") : [];
    const nav = h("nav", { class: "mp-crumbs", "aria-label": "Folder" },
      h("button", { class: "mp-crumb" + (parts.length ? "" : " here"), onclick: () => drill("") }, S.data.tree.name || "repo"));
    parts.forEach((p, i) => {
      const to = parts.slice(0, i + 1).join("/");
      nav.append(h("span", { class: "mp-sep", "aria-hidden": "true" }, "›"),
        h("button", { class: "mp-crumb" + (i === parts.length - 1 ? " here" : ""), onclick: () => drill(to) }, p));
    });
    const node = I.nodes.get(S.path) || S.data.tree;
    const agg = I.agg.get(node.path);
    nav.append(h("span", { class: "mp-crumb-meta" }, `${agg.files} files · ${node.lines.toLocaleString()} lines`));
    if (parts.length) nav.append(h("button", { class: "btn ghost sm mp-up", onclick: () => drill(parentOf(S.path)), title: "Up a folder (Esc)" }, "↑ Up"));
    return nav;
  }

  function drill(p) {
    if (!I.nodes.has(p)) return;
    S.path = p;
    render();
    requestAnimationFrame(() => el.querySelector(".mp-crumb.here")?.focus());
  }

  // A folder whose only child is a folder reads as one: web/src.
  function collapse(n) {
    let label = n.name;
    while (n.children && n.children.length === 1 && n.children[0].children) { n = n.children[0]; label += "/" + n.name; }
    return { node: n, label };
  }

  // ------------------------------------------------------------- treemap
  let drawn = new Map();
  function drawTree() {
    if (!tm || !tm.isConnected) return;
    const W = tm.clientWidth, H = tm.clientHeight;
    tm.replaceChildren();
    drawn = new Map();
    if (W < 40 || H < 40) return;
    const node = I.nodes.get(S.path) || S.data.tree;
    layout(node, { x: 0, y: 0, w: W, h: H }, 0);
    drawCollisionLabels();
    drawArrows(W, H);
    drawPins();
  }

  function layout(node, r, depth) {
    const kids = (node.children || []).filter((c) => c.lines > 0);
    const gap = depth === 0 ? 3 : 2;
    for (const p of squarify(kids.map((c) => ({ item: c, value: c.lines })), r)) {
      drawNode(p.item, { x: p.x + gap, y: p.y + gap, w: p.w - 2 * gap, h: p.h - 2 * gap }, depth);
    }
  }

  function dominant(by) {
    let best = null, n = 0;
    for (const [id, v] of by) if (v > n) { n = v; best = id; }
    return best ? I.agents.get(best) : null;
  }

  function drawNode(n0, r, depth) {
    if (r.w < 3 || r.h < 3) return;
    const pos = { left: Math.round(r.x) + "px", top: Math.round(r.y) + "px", width: Math.round(r.w) + "px", height: Math.round(r.h) + "px" };
    if (n0.kind === "more" || n0.name === "…" || n0.folded) {
      // GET /api/map folds small files into {kind: "more", files: n}.
      const k = n0.files || n0.folded || "";
      const can = Array.isArray(n0.items) && n0.items.length > 0;
      // Opened, its files get tiles of their own and a list beside the map,
      // where a tile too small to read is still one click away.
      const open = () => { const dir = parentOf(n0.path); S.unfold.add(dir); tip.hide(); index(); select({ type: "fold", path: dir, items: n0.items }); };
      tm.append(h("div", {
        class: "mp-c mp-fold" + (can ? " can" : ""), style: pos, "data-path": n0.path, "data-folded": k || "?",
        role: can ? "button" : null, tabindex: can ? "0" : null, "aria-label": `${k} small files folded together${can ? ". Open them." : ""}`,
        onclick: can ? (e) => { e.stopPropagation(); open(); } : null,
        onkeydown: can ? (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); e.stopPropagation(); open(); } } : null,
      }, r.w > 40 && r.h > 16 ? h("span", { class: "mp-cn" }, `… ${k} small${can ? " · open" : ""}`) : null));
      return;
    }
    if (!n0.children) return drawFile(n0, r, pos);
    const { node, label } = collapse(n0);
    const agg = I.agg.get(node.path);
    const lead = dominant(agg.by);
    const big = depth === 0 || (r.w > 150 && r.h > 90);
    const headH = big ? 40 : 22;
    const touched = agg.by.size > 0;
    const dim = S.focus && !agg.by.has(S.focus);
    const f = h("div", {
      class: `mp-f d${Math.min(depth, 2)}` + (touched ? " on" : "") + (dim ? " dim" : ""), style: pos,
      "data-path": node.path, "data-folder": "1", role: "button", tabindex: "0",
      "aria-label": `${label}: ${agg.files} files, ${node.lines} lines${touched ? `, changed by ${[...agg.by.keys()].map((k) => nameOf(I.agents.get(k))).join(" and ")}` : ", untouched"}. Open folder.`,
    });
    if (lead) { f.style.setProperty("--t", agentTint(lead, depth ? 0.07 : 0.1)); f.style.setProperty("--b", agentTint(lead, 0.6)); }
    const avs = [...agg.by.entries()].sort((a, b) => b[1] - a[1]).slice(0, 3).map(([id]) => avatar(I.agents.get(id), big ? 20 : 15));
    const subText = agg.changedFiles.length
      ? [...new Set(agg.changedFiles.map(base))].slice(0, 4).join(" · ")
      : (I.read.size && [...I.read.keys()].some((p) => under(p, node.path)) ? "read lately" : "untouched");
    f.append(h("div", { class: "mp-fh", style: { height: headH + "px" } },
      h("div", { class: "mp-fn" }, h("b", null, label), big && r.h > 56 ? h("span", { class: "mp-fs" }, subText) : null),
      r.w > 90 ? h("span", { class: "mp-avs" }, avs) : null));
    f.addEventListener("click", () => drill(node.path));
    f.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); drill(node.path); } });
    tm.append(f);
    const content = { x: r.x + 4, y: r.y + headH, w: r.w - 8, h: r.h - headH - 4 };
    drawn.set(node.path, { r, content, el: f, folder: true });
    if (depth < 3 && content.w > 56 && content.h > 34) layout(node, content, depth + 1);
    else f.classList.add("leaf");
  }

  function drawFile(n, r, pos) {
    const ch = I.changed.get(n.path) || [];
    const rd = I.read.get(n.path) || [];
    const coll = I.coll.get(n.path);
    const dim = S.focus && !ch.some((c) => c.agent.id === S.focus) && !rd.some((a) => a.id === S.focus);
    const sel = S.sel && S.sel.type === "file" && S.sel.path === n.path;
    const c = h("div", {
      class: "mp-c" + (ch.length ? " on" : "") + (coll ? " coll" : "") + (rd.length && !ch.length ? " rd" : "") + (dim ? " dim" : "") + (sel ? " sel" : ""),
      style: pos, "data-path": n.path, role: "button", tabindex: "0",
      "aria-label": `${n.path}, ${n.lines} lines${ch.length ? ", changed by " + ch.map((x) => nameOf(x.agent)).join(" and ") : ""}${coll ? ", collision" : ""}`,
    });
    if (ch.length) { c.style.setProperty("--t", agentTint(ch[0].agent, 0.28)); c.style.setProperty("--b", agentTint(ch[0].agent, 0.85)); }
    else if (rd.length) c.style.setProperty("--b", agentColor(rd[0]));
    if (r.w > 44 && r.h > 17) c.append(h("span", { class: "mp-cn" }, n.name));
    if (r.w > 64 && r.h > 34) c.append(h("span", { class: "mp-cl" }, ch.length ? ch.map((x) => `+${x.added} −${x.removed}`).join(" · ") : `${n.lines} lines`));
    if (ch.length && r.w > 52 && r.h > 34) c.append(h("span", { class: "mp-cav" }, ch.slice(0, 2).map((x) => avatar(x.agent, 15))));
    c.addEventListener("click", (e) => { e.stopPropagation(); select({ type: "file", path: n.path }); });
    c.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); e.stopPropagation(); select({ type: "file", path: n.path }); } });
    tm.append(c);
    drawn.set(n.path, { r, el: c });
  }

  // The deepest drawn box for a path: the file itself, or a folder holding it.
  function boxFor(p) {
    for (let q = p; ; q = parentOf(q)) {
      if (drawn.has(q)) return { ...drawn.get(q), exact: q === p };
      if (!q) return null;
    }
  }

  function drawCollisionLabels() {
    for (const c of S.data.collisions || []) {
      if (!under(c.path, S.path) && !under(S.path, c.path)) continue;
      const b = boxFor(c.path);
      if (!b) continue;
      if (!b.exact) b.el.classList.add("coll");
      // The label goes in the nearest folder that has room for it.
      let fb = null;
      for (let q = parentOf(c.path); ; q = parentOf(q)) { const d = drawn.get(q); if (d && d.folder && d.r.w > 150) { fb = d; break; } if (!q) break; }
      if (fb) fb.el.classList.add("hot");
      const host = fb || b;
      const names = c.agents.map((id) => nameOf(I.agents.get(id)));
      const text = c.kind === "both_changed" ? `${names.join(" + ")} both in ${base(c.path)}` : `${names[1] || "?"} heading into ${base(c.path)}`;
      const lab = h("button", { class: "mp-clab", onclick: (e) => { e.stopPropagation(); select({ type: "file", path: c.path }); } }, text);
      Object.assign(lab.style, { left: Math.round(host.r.x + 8) + "px", top: Math.round(host.r.y + host.r.h - 28) + "px", maxWidth: Math.round(host.r.w - 16) + "px" });
      tm.append(lab);
    }
  }

  function centre(b) { return { x: b.r.x + b.r.w / 2, y: b.r.y + b.r.h / 2 }; }
  function drawArrows(W, H) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "mp-arrows");
    svg.setAttribute("width", W); svg.setAttribute("height", H);
    for (const c of S.data.collisions || []) {
      if (c.kind !== "heading_into") continue;
      const reader = I.agents.get(c.agents[1]);
      const to = boxFor(c.path);
      if (!reader || !to) continue;
      const own = (S.data.agents || []).find((r) => r.agent.id === reader.id);
      const pts = (own?.files || []).map((f) => boxFor(f.path)).filter((b) => b && b !== to && b.el !== to.el).map(centre);
      if (!pts.length) continue;
      const a = { x: pts.reduce((s, p) => s + p.x, 0) / pts.length, y: pts.reduce((s, p) => s + p.y, 0) / pts.length };
      const b = centre(to);
      if (Math.hypot(a.x - b.x, a.y - b.y) < 30) continue;
      const mx = (a.x + b.x) / 2, my = Math.min(a.y, b.y) - 40;
      const col = agentColor(reader);
      const path = document.createElementNS(svg.namespaceURI, "path");
      path.setAttribute("d", `M${a.x},${a.y} Q${mx},${my} ${b.x},${b.y}`);
      Object.entries({ stroke: col, "stroke-width": 2, fill: "none", "stroke-dasharray": "6 5", "stroke-linecap": "round" }).forEach(([k, v]) => path.setAttribute(k, v));
      const dot = (p, r) => { const e = document.createElementNS(svg.namespaceURI, "circle"); e.setAttribute("cx", p.x); e.setAttribute("cy", p.y); e.setAttribute("r", r); e.setAttribute("fill", col); return e; };
      svg.append(path, dot(b, 4.5), dot(a, 3));
    }
    tm.append(svg);
  }

  function drawPins() {
    const groups = new Map();
    for (const p of S.data.pins || []) {
      if (!under(p.path, S.path)) continue;
      const b = boxFor(p.path);
      if (!b) continue;
      const key = b.el;
      if (!groups.has(key)) groups.set(key, { b, pins: [] });
      groups.get(key).pins.push(p);
    }
    for (const { b, pins } of groups.values()) {
      pins.forEach((p, i) => {
        const n = I.nodes.get(p.path);
        const r = b.content || b.r;
        let x = r.x + r.w / 2 + (i - (pins.length - 1) / 2) * 22;
        let y = r.y + r.h / 2;
        if (b.exact && p.line != null && n && n.lines) {
          // Down the file as far as its line, below the cell's name when there is room.
          const top = r.h > 70 ? 40 : 12;
          y = r.y + Math.max(top, Math.min(r.h - 12, (p.line / n.lines) * r.h));
        }
        else if (!b.exact) { const k = hash(p.path); x = r.x + r.w * (0.2 + 0.6 * ((k % 97) / 97)); y = r.y + r.h * (0.25 + 0.5 * (((k >> 7) % 89) / 89)); }
        const sel = S.sel && S.sel.type === "pin" && S.sel.pin === p;
        const pin = h("button", {
          class: "mp-pin" + (sel ? " sel" : "") + (S.focus && (!p.agent || p.agent.id !== S.focus) ? " dim" : ""),
          style: { left: Math.round(x) + "px", top: Math.round(y) + "px" },
          "aria-label": `${p.record.kind}: ${p.record.text} (${p.path}${p.line != null ? ":" + p.line : ""})`,
        });
        pin.addEventListener("click", (e) => { e.stopPropagation(); openPin(p); });
        pin.addEventListener("mouseenter", (e) => tip.show(pinTip(p), e.clientX, e.clientY));
        pin.addEventListener("mouseleave", () => tip.hide());
        pin.addEventListener("focus", () => { const rr = pin.getBoundingClientRect(); tip.show(pinTip(p), rr.right, rr.bottom); });
        pin.addEventListener("blur", () => tip.hide());
        tm.append(pin);
      });
    }
  }
  const hash = (s) => { let x = 7; for (const c of s) x = (x * 31 + c.charCodeAt(0)) >>> 0; return x; };

  // ------------------------------------------------------------- tooltips
  function hover(e) {
    if (e.target.closest(".mp-pin")) return;
    const t = e.target.closest("[data-path]");
    if (!t) return tip.hide();
    showTip(t, e);
  }
  function showTip(t, e) {
    const p = t.dataset.path;
    const n = I.nodes.get(p);
    if (!n && !t.dataset.folded) return;
    const r = t.getBoundingClientRect();
    const x = e ? e.clientX : r.left + 12, y = e ? e.clientY : r.top + 12;
    if (t.dataset.folded) {
      const fold = I.nodes.get(parentOf(p))?.children?.find((c) => c.path === p);
      const names = (fold?.items || []).slice(0, 8).map((f) => f.name);
      return tip.show([h("b", null, "Small files"), h("div", { class: "lv-sub" }, `${t.dataset.folded} files folded together${names.length ? `: ${names.join(", ")}${fold.items.length > 8 ? ", …" : ""}` : ""}`),
        names.length ? h("div", { class: "lv-sub" }, "Click to open them.") : null], x, y);
    }
    const agg = I.agg.get(p);
    const rows = [h("div", { class: "mp-tp" }, p || n.name),
      h("div", { class: "lv-sub" }, n.children ? `${agg.files} files · ${n.lines.toLocaleString()} lines` : `${n.lines.toLocaleString()} lines`)];
    const changed = n.children
      ? [...agg.by.entries()].sort((a, b) => b[1] - a[1]).map(([id, v]) => ({ agent: I.agents.get(id), text: `${v} lines changed` }))
      : (I.changed.get(p) || []).map((c) => ({ agent: c.agent, text: `+${c.added} −${c.removed}` }));
    if (changed.length) rows.push(h("div", { class: "mp-tk" }, "Changed by"), ...changed.map((c) => h("div", { class: "mp-tr" }, avatar(c.agent, 15), h("b", null, nameOf(c.agent)), h("span", { class: "lv-sub" }, c.text))));
    const readers = n.children ? [...new Set([...I.read.entries()].filter(([q]) => under(q, p)).flatMap(([, a]) => a))] : I.read.get(p) || [];
    if (readers.length) rows.push(h("div", { class: "mp-tk" }, "Read lately by"), h("div", { class: "mp-tr" }, readers.map((a) => [avatar(a, 15), h("span", null, nameOf(a))])));
    if (!changed.length && !readers.length) rows.push(h("div", { class: "lv-sub" }, "No agent is here."));
    const coll = n.children ? agg.coll : I.coll.get(p) ? [I.coll.get(p)] : [];
    for (const c of coll) rows.push(h("div", { class: "mp-tc" }, c.detail || c.kind));
    if (agg.pins) rows.push(h("div", { class: "mp-tr" }, ctxChip(`${agg.pins} decision${agg.pins > 1 ? "s" : ""} ChloeCtx can explain`)));
    tip.show(rows, x, y);
  }
  function pinTip(p) {
    return [h("div", { class: "mp-tr" }, ctxChip(p.record.kind), h("span", { class: "lv-sub mono" }, `${base(p.path)}${p.line != null ? ":" + p.line : ""}`)),
      h("div", { class: "mp-tt" }, p.record.text),
      h("div", { class: "lv-sub" }, p.line != null ? "Click for why this line is like this" : "Click for the record")];
  }

  // ------------------------------------------------------------ the side
  function openPin(p) {
    tip.hide();
    if (p.line != null) {
      const args = p.agent ? { agent: p.agent.id, path: p.path, line: p.line } : { repo: repoOf(ctx), path: p.path, line: p.line };
      S.sel = { type: "pin", pin: p };
      render();
      ctx.openWhy(args);
      return;
    }
    select({ type: "pin", pin: p });
  }

  function select(sel) {
    S.sel = sel;
    if (sel && sel.type === "file" && !under(sel.path, S.path)) S.path = "";
    render();
    requestAnimationFrame(() => el.querySelector(".mp-selcard")?.focus());
  }

  function closeSel() { S.sel = null; render(); }

  function collisionsCard() {
    const list = S.data.collisions || [];
    if (!list.length) return null;
    return h("section", { class: "lv-card mp-coll" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2 mp-crit" }, `${list.length} collision${list.length > 1 ? "s" : ""}`),
        h("span", { class: "lv-sub" }, "from the agents' diffs and reads")),
      list.map((c) => {
        const [a, b] = c.agents.map((id) => I.agents.get(id) || { id, name: id });
        const both = c.kind === "both_changed";
        // both_changed: the second agent waits for the first; heading_into:
        // the reader (second) hears that the changer (first) moved the file.
        const target = b, other = a;
        const key = `${c.path}:${target.id}`;
        const sent = S.sent.get(key);
        const msg = both
          ? `${nameOf(other)} is changing ${c.path} too (${c.detail || "both of your diffs touch it"}). Leave ${c.path} to ${nameOf(other)} until it is done, then rebase on its branch wb/${nameOf(other)}.`
          : `${nameOf(other)} changed ${c.path} since you read it (${c.detail || "it moved under you"}). Read it again from ${nameOf(other)}'s worktree before you code against it.`;
        return h("div", { class: "mp-ci" },
          h("div", { class: "mp-cr" }, pill(both ? "both changed" : "heading into", "crit"), h("code", { class: "mp-cp" }, c.path)),
          h("div", { class: "mp-cd" }, agentChip(a, ctx), h("span", { class: "lv-sub" }, both ? "and" : "→"), agentChip(b, ctx)),
          c.detail ? h("p", { class: "mp-ct" }, c.detail) : null,
          h("div", { class: "mp-ca" },
            h("button", {
              class: "btn sm primary", disabled: sent || S.busy.has(key) ? true : null,
              onclick: async () => {
                S.busy.add(key); render();
                try { await ctx.api(`/api/agents/${encodeURIComponent(target.id)}/steer`, { method: "POST", body: { text: msg } }); S.sent.set(key, "sent"); }
                catch (e) { S.steerErr = { key, text: e.message || String(e) }; }
                S.busy.delete(key); render();
              },
            }, sent ? `Told ${nameOf(target)} ✓` : both ? `Ask ${nameOf(target)} to wait for ${nameOf(other)}` : `Tell ${nameOf(target)} it moved`),
            h("button", { class: "btn sm", onclick: () => { S.path = parentOf(parentOf(c.path)); select({ type: "file", path: c.path }); } }, "Show"),
            h("button", { class: "btn sm ghost", onclick: () => ctx.zoom(target.id), title: `Open ${nameOf(target)}'s terminal` }, "Terminal")),
          S.steerErr && S.steerErr.key === key ? h("div", { class: "err" }, S.steerErr.text) : null);
      }));
  }

  function selectionCard() {
    if (!S.sel) { dropEsc?.(); dropEsc = null; return null; }
    dropEsc?.(); dropEsc = onEscape(closeSel);
    const close = h("button", { class: "btn ghost icon sm", "aria-label": "Close (Esc)", title: "Close (Esc)", onclick: closeSel }, "×");
    if (S.sel.type === "pin") {
      const p = S.sel.pin, r = p.record;
      return h("section", { class: "lv-card mp-selcard mp-rec", tabindex: "-1" },
        h("div", { class: "lv-card-head" }, ctxChip(`${r.kind} · from ChloeCtx`), close),
        h("p", { class: "mp-rq" }, r.text),
        h("dl", { class: "mp-kv" },
          h("dt", null, "File"), h("dd", null, h("code", null, p.path + (p.line != null ? ":" + p.line : ""))),
          h("dt", null, "Recorded"), h("dd", null, r.ts ? `${fmtAgo(r.ts)} · ${new Date(r.ts).toLocaleString([], { day: "numeric", month: "short", hour: "numeric", minute: "2-digit" })}` : "—"),
          h("dt", null, "By"), h("dd", null, p.agent ? agentChip(p.agent, ctx) : h("span", { class: "lv-sub" }, "an earlier session, before Workbench")),
          h("dt", null, "Record"), h("dd", null, h("code", null, r.id))),
        h("div", { class: "mp-ca" },
          h("button", { class: "btn sm", onclick: () => { S.path = parentOf(p.path); S.sel = { type: "file", path: p.path }; render(); } }, "Show the file"),
          p.line != null ? h("button", { class: "btn sm primary", onclick: () => openPin(p) }, "Why is this line like this?") : null));
    }
    if (S.sel.type === "fold") {
      const f = S.sel, items = [...f.items].sort((a, b) => (b.lines || 0) - (a.lines || 0) || a.name.localeCompare(b.name));
      return h("section", { class: "lv-card mp-selcard", tabindex: "-1" },
        h("div", { class: "lv-card-head" }, h("code", { class: "mp-fp" }, `${f.path || "/"} · ${items.length} small files`), close),
        h("div", { class: "lv-sub" }, "Too small for a readable tile; pick one to see who changed it."),
        h("ul", { class: "mp-fl" }, items.map((it) => h("li", null,
          h("button", { class: "mp-fi", onclick: () => select({ type: "file", path: it.path }) },
            h("code", null, it.name), h("span", { class: "lv-sub" }, `${it.lines} line${it.lines === 1 ? "" : "s"}`),
            I.changed.has(it.path) ? h("span", { class: "mp-num" }, I.changed.get(it.path).map((c) => c.agent.name).join(", ")) : null)))),
        h("div", { class: "mp-ca" }, h("button", { class: "btn sm ghost", onclick: () => { S.unfold.delete(f.path); S.sel = null; load(); } }, "Fold them again")));
    }
    const p = S.sel.path, n = I.nodes.get(p);
    if (!n) return null;
    const ch = I.changed.get(p) || [], rd = I.read.get(p) || [], pins = I.pins.get(p) || [], coll = I.coll.get(p);
    return h("section", { class: "lv-card mp-selcard", tabindex: "-1" },
      h("div", { class: "lv-card-head" }, h("code", { class: "mp-fp" }, p), close),
      h("div", { class: "lv-sub" }, `${n.lines.toLocaleString()} lines in the main checkout`),
      coll ? h("p", { class: "mp-ct mp-crit" }, coll.detail || coll.kind) : null,
      ch.length ? h("div", { class: "mp-sl" }, ch.map((c) => h("div", { class: "mp-tr" }, agentChip(c.agent, ctx),
        h("span", { class: "mp-num" }, h("span", { class: "n-add" }, `+${c.added}`), " ", h("span", { class: "n-del" }, `−${c.removed}`)),
        h("button", { class: "lv-link", onclick: () => ctx.zoom(c.agent.id) }, "terminal")))) : h("div", { class: "lv-sub" }, "No agent changed it."),
      rd.length ? h("div", { class: "mp-tr" }, h("span", { class: "lv-sub" }, "Read lately by"), rd.map((a) => agentChip(a, ctx))) : null,
      pins.length ? h("div", { class: "mp-sl" }, pins.map((q) => h("button", { class: "mp-pr", onclick: () => openPin(q) }, ctxChip(q.record.kind), h("span", null, q.record.text)))) : null);
  }

  function pinsCard() {
    const pins = S.data.pins || [];
    if (!pins.length) return card(null, null, h("div", { class: "lv-sub" }, "ChloeCtx has no decision that names a file of this repository yet."));
    // A record that names several files is pinned on each; listed once.
    const byRec = new Map();
    for (const p of pins.filter((p) => under(p.path, S.path))) {
      const k = p.record.id || p.record.text;
      if (byRec.has(k)) byRec.get(k).paths.push(p);
      else byRec.set(k, { ...p, paths: [p] });
    }
    const here = [...byRec.values()];
    return h("section", { class: "lv-card" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2 mp-vio" }, h("span", { class: "mp-dia" }), ` ${here.length} decision${here.length === 1 ? "" : "s"} ${S.path ? "in " + S.path : "on this map"}`)),
      here.length ? h("div", { class: "mp-plist" }, here.map((p) => h("button", {
        class: "mp-pr" + (S.sel && p.paths.includes(S.sel.pin) ? " on" : ""), onclick: () => openPin(p.paths[0]),
        onmouseenter: () => el.querySelectorAll(".mp-pin").forEach((x) => x.classList.toggle("hi", x.getAttribute("aria-label").includes(p.record.text))),
        onmouseleave: () => el.querySelectorAll(".mp-pin.hi").forEach((x) => x.classList.remove("hi")),
      },
      h("span", { class: "mp-pt" }, p.record.text),
      h("span", { class: "mp-pm" }, p.paths.map((q, i) => [i ? ", " : null, h("code", null, base(q.path) + (q.line != null ? ":" + q.line : ""))]), " · ", p.record.kind,
        p.agent ? [" · ", p.agent.name] : "", p.line != null ? h("span", { class: "mp-why" }, "Why?") : null)))) : h("div", { class: "lv-sub" }, "None in this folder."));
  }

  load();
  return {
    update(state) { if (state) ctx.state = { ...ctx.state, ...state }; refresh(); },
    unmount() {
      alive = false;
      refresh.cancel();
      ro?.disconnect();
      cancelAnimationFrame(frame);
      dropEsc?.(); dropUp?.();
      tip.remove();
      el.remove();
    },
  };
}

useStyle("map", `
.mp{display:flex;flex-direction:column;gap:10px}
.mp-dia{display:inline-block;width:8px;height:8px;border-radius:2px;background:var(--ctx);transform:rotate(45deg);margin:0 3px;vertical-align:1px}
.mp-legend{display:flex;flex-wrap:wrap;gap:6px;align-items:center}
.mp-key{display:inline-flex;align-items:center;gap:7px;height:28px;padding:0 10px 0 8px;border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--text-2);cursor:pointer;font:inherit}
.mp-key i{width:12px;height:12px;border-radius:4px;background:var(--c)}
.mp-key-n{color:var(--text);font-weight:600}.mp-key-d{font:11px var(--mono);color:var(--muted)}
.mp-key:hover{border-color:var(--line-2)}.mp-key.on{border-color:var(--c);box-shadow:0 0 0 2px var(--focus)}
.mp-key-pin{display:inline-flex;align-items:center;gap:4px;color:var(--ctx);font-weight:600;font-size:12px;margin-left:6px}
.mp-body{flex:1 1 auto;min-height:540px;display:grid;grid-template-columns:minmax(0,1fr) 384px;gap:18px}
.mp-left{display:flex;flex-direction:column;gap:8px;min-width:0;min-height:0}
.mp-crumbs{display:flex;align-items:center;gap:4px;flex-wrap:wrap;min-height:30px}
.mp-crumb{border:0;background:none;color:var(--accent);font:inherit;font-weight:600;padding:3px 6px;border-radius:6px;cursor:pointer}
.mp-crumb:hover{background:var(--surface-3)}.mp-crumb.here{color:var(--text)}
.mp-sep{color:var(--muted)}.mp-crumb-meta{color:var(--muted);margin-left:8px;font-size:12px}.mp-up{margin-left:auto}
.mp-tm{position:relative;flex:1 1 auto;min-height:440px;border-radius:12px;background:var(--sunken);border:1px solid var(--line);overflow:hidden}
.mp-f{position:absolute;border:1px solid var(--line);border-radius:9px;background:var(--surface);cursor:pointer;overflow:hidden}
.mp-f.on{background:var(--t);border-color:var(--b)}
.mp-f.d1{border-radius:7px}.mp-f.d2{border-radius:6px}
.mp-f:hover{border-color:var(--text-2)}
.mp-f.coll,.mp-f.hot{border:2px solid var(--crit)}
.mp-f.coll{background-image:repeating-linear-gradient(45deg,color-mix(in srgb,var(--crit) 26%,transparent) 0 7px,transparent 7px 14px)}
.mp-f.dim,.mp-c.dim{opacity:.35}
.mp-fh{display:flex;align-items:flex-start;justify-content:space-between;gap:6px;padding:7px 9px 0;min-width:0}
.mp-f.d1 .mp-fh,.mp-f.d2 .mp-fh{padding:3px 7px 0;font-size:12px}
.mp-fn{display:flex;flex-direction:column;min-width:0}
.mp-fn b{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;font-weight:650}
.mp-f:not(.on) .mp-fn b{color:var(--text-2)}
.mp-fs{font:10.5px var(--mono);color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;margin-top:2px}
.mp-avs{display:inline-flex;gap:3px;flex:none}
.mp-f.leaf .mp-fn{white-space:normal}
.mp-c{position:absolute;border:1px solid var(--line);border-radius:5px;background:var(--surface-2);cursor:pointer;overflow:hidden;padding:3px 6px;display:flex;flex-direction:column;gap:1px}
.mp-c.on{background:var(--t);border-color:var(--b)}
.mp-c.rd{border:1.5px dashed var(--b)}
.mp-c.coll{border:2px solid var(--crit);background-image:repeating-linear-gradient(45deg,color-mix(in srgb,var(--crit) 34%,transparent) 0 6px,transparent 6px 12px)}
.mp-c.sel{box-shadow:0 0 0 2px var(--accent)}
.mp-c:hover{filter:brightness(1.06);border-color:var(--text-2)}
.mp-cn{font:11px/1.25 var(--mono);color:var(--text);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.mp-c:not(.on) .mp-cn{color:var(--muted)}
.mp-cl{font:10px var(--mono);color:var(--muted);white-space:nowrap}
.mp-c.on .mp-cl{color:var(--text-2)}
.mp-cav{position:absolute;right:4px;bottom:4px;display:flex;gap:2px}
.mp-fold{background:transparent;border-style:dashed;cursor:default}
.mp-fold.can{cursor:pointer}.mp-fold.can:hover,.mp-fold.can:focus-visible{background:var(--surface-2);border-color:var(--accent)}
.mp .lv-head .rp-switch{margin-left:auto}
.mp-clab{position:absolute;z-index:3;border:0;border-radius:6px;padding:2px 8px;background:var(--crit-soft);color:var(--crit-ink);font:600 11.5px var(--sans);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;cursor:pointer;box-shadow:0 0 0 1px color-mix(in srgb,var(--crit) 40%,transparent)}
.mp-arrows{position:absolute;left:0;top:0;pointer-events:none;z-index:2}
.mp-pin{position:absolute;z-index:4;width:13px;height:13px;margin:-6.5px 0 0 -6.5px;padding:0;border:0;border-radius:3px;background:var(--ctx);transform:rotate(45deg);cursor:pointer;box-shadow:0 0 0 2.5px var(--surface),0 0 12px color-mix(in srgb,var(--ctx) 70%,transparent);transition:transform .12s}
.mp-pin:hover,.mp-pin:focus-visible,.mp-pin.hi{transform:rotate(45deg) scale(1.35)}
.mp-pin.sel{width:17px;height:17px;margin:-8.5px 0 0 -8.5px;background:var(--surface);box-shadow:0 0 0 3px var(--ctx),0 0 20px var(--ctx)}
.mp-pin.dim{opacity:.35}
.mp-side{display:flex;flex-direction:column;gap:12px;min-height:0;overflow:auto;padding-bottom:4px}
.mp-coll{border-color:color-mix(in srgb,var(--crit) 45%,var(--line))}
.mp-crit{color:var(--crit-ink)}.mp-vio{color:var(--ctx);display:flex;align-items:center;gap:4px}
.mp-ci{padding:10px 0;border-top:1px solid var(--line)}.mp-ci:first-of-type{border-top:0;padding-top:0}
.mp-cr{display:flex;align-items:center;gap:8px;min-width:0}
.mp-cp{font:11.5px var(--mono);color:var(--text-2);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.mp-cd{display:flex;align-items:center;gap:8px;margin-top:8px}
.mp-ct{margin:6px 0 0;color:var(--text-2)}
.mp-ca{display:flex;flex-wrap:wrap;gap:6px;margin-top:10px}
.mp-fl{list-style:none;margin:8px 0 0;padding:0;max-height:420px;overflow:auto;border-top:1px solid var(--line)}
.mp-fi{display:flex;gap:10px;align-items:baseline;width:100%;padding:5px 2px;border:0;border-bottom:1px solid var(--line);background:none;color:var(--text);font:inherit;text-align:left;cursor:pointer}
.mp-fi:hover,.mp-fi:focus-visible{background:var(--surface-2)}.mp-fi code{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis}
.mp-selcard{outline:none;border-color:color-mix(in srgb,var(--ctx) 40%,var(--line))}
.mp-rq{margin:0;font-size:15px;line-height:1.45;font-weight:600}
.mp-kv{display:grid;grid-template-columns:76px 1fr;gap:6px 10px;margin:12px 0 0;font-size:12.5px}
.mp-kv dt{color:var(--muted);font-weight:650;font-size:11px;text-transform:uppercase;letter-spacing:.05em;padding-top:2px}
.mp-kv dd{margin:0;min-width:0;overflow-wrap:anywhere}.mp-kv code,.mp-fp{font:12px var(--mono)}
.mp-fp{overflow-wrap:anywhere}
.mp-sl{display:flex;flex-direction:column;gap:6px;margin-top:10px}
.mp-num{font-variant-numeric:tabular-nums}
.mp-plist{display:flex;flex-direction:column}
.mp-pr{display:flex;flex-direction:column;gap:3px;text-align:left;border:0;border-top:1px solid var(--line);background:none;padding:9px 4px;color:var(--text);font:inherit;cursor:pointer;border-radius:0}
.mp-pr:first-child{border-top:0}
.mp-pr:hover,.mp-pr.on{background:var(--surface-2)}
.mp-sl .mp-pr{flex-direction:row;align-items:baseline;gap:8px;border:0;padding:4px}
.mp-pt{display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.mp-pm{font-size:11.5px;color:var(--muted);display:flex;gap:4px;align-items:baseline}.mp-pm code{font:11px var(--mono)}
.mp-why{margin-left:auto;color:var(--accent);font-weight:600}
.mp-tp{font:600 12px var(--mono);overflow-wrap:anywhere}
.mp-tk{margin-top:7px;color:var(--muted);font-size:10.5px;font-weight:650;text-transform:uppercase;letter-spacing:.05em}
.mp-tr{display:flex;align-items:center;gap:6px;flex-wrap:wrap;margin-top:4px}
.mp-tc{margin-top:7px;padding:5px 7px;border-radius:6px;background:var(--crit-soft);color:var(--crit-ink)}
.mp-tt{margin-top:5px;font-weight:600}
@media (max-width:1180px){.mp-body{grid-template-columns:minmax(0,1fr) 320px}}
@media (max-width:860px){.mp-body{grid-template-columns:1fr;min-height:0}.mp-tm{flex:none;height:min(520px,70vh);min-height:360px}.mp-side{overflow:visible}}
`);
