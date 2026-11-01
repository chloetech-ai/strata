// A JSON field's paths as a tree: each key its objects hold, how many rows
// hold it (a bar of their share), the types of its values (a bar of
// strings, numbers and booleans) and what they are (a range, the values),
// read from the field's index by GET /v1/tables/:t/paths, without reading
// a row. Click a path for its values, and to facet, filter or query it.

import { h, icon, clear, fmt, empty, failure, copy } from "./core.js";
import { paths, PATH_TYPES, pathSql, pathType } from "./catalog.js";
import { quoteName } from "./sqledit.js";

const KIND = { float: "number", keyword: "string", bool: "bool" };

/** A value as the listing gives it, as SQL writes it. */
export const valueSql = (v) => typeof v === "string" ? `'${v.replace(/'/g, "''")}'` : String(v);

/** A short line on what a path holds: its range, its values, or its keys. */
export function pathSummary(p, kids = 0) {
  const ty = pathType(p);
  if (!ty) return kids ? `object · ${fmt.plural(kids, "key")}` : "empty objects or long strings";
  const parts = [];
  if (p.types.number && p.min != null) parts.push(p.min === p.max ? fmt.n(p.min, 3) : `${fmt.n(p.min, 3)} – ${fmt.n(p.max, 3)}`);
  if (p.distinct != null) parts.push(fmt.plural(p.distinct, "value"));
  else parts.push("100+ values");
  if (kids) parts.push(`${fmt.plural(kids, "key")} below`);
  return parts.join(" · ");
}

/** The mix of a path's types: a bar of strings, numbers and booleans. */
export function typeMix(p) {
  const t = p.types || {};
  const all = PATH_TYPES.reduce((a, [k]) => a + (t[k] || 0), 0);
  const tip = all ? PATH_TYPES.filter(([k]) => t[k]).map(([k]) => `${fmt.si(t[k])} rows with a ${k}`).join(", ") : "no values: objects only";
  return h("span", { class: "pt-mix", title: tip, "aria-label": tip },
    all ? PATH_TYPES.filter(([k]) => t[k]).map(([k, c]) => h("i", { style: { flexGrow: t[k], background: c } })) : h("i", { class: "obj" }));
}

/** The paths of a listing as a tree: [{key, keys, path, p (null for a key only implied), kids: [...]}]. */
export function pathNodes(listing) {
  const root = { kids: new Map() };
  for (const p of listing.paths || []) {
    let n = root;
    p.keys.forEach((k, i) => {
      if (!n.kids.has(k)) n.kids.set(k, { key: k, keys: p.keys.slice(0, i + 1), p: null, kids: new Map() });
      n = n.kids.get(k);
    });
    n.p = p;
  }
  const out = (n) => [...n.kids.values()].sort((a, b) => a.key.localeCompare(b.key)).map((c) => ({ ...c, kids: out(c) }));
  return out(root);
}

/**
 * pathTree({table, field}): the tree of a JSON field's paths, read when
 * shown; shares are of the rows the listing read (the table's).
 */
export function pathTree({ table, field }) {
  const q = h("input", { class: "input sm", type: "search", placeholder: "Filter paths…", "aria-label": `Filter the paths of ${field}` });
  const head = h("span", { class: "small dim grow" });
  const list = h("div", { class: "pt-list", role: "tree", "aria-label": `Paths of ${field}` });
  const box = h("div", { class: "pt" }, h("div", { class: "row nw", style: { gap: "8px" } }, q, head,
    h("button", { class: "btn sm ghost", title: "Read the paths again", onclick: () => load(true) }, icon("refresh"))), list);
  let listing = null, shut = new Set(), on = null;
  q.oninput = () => draw();

  function load(force) {
    clear(list, h("div", { class: "muted small", style: { padding: "8px 2px" } }, "Reading the paths from the index…"));
    paths(table, field, force).then((l) => { listing = l; draw(); }).catch((e) => clear(list, failure(e, `the paths of ${field}`)));
  }
  function draw() {
    const ps = listing.paths || [];
    clear(head, `${fmt.plural(ps.length, "path")} · ${fmt.si(listing.rows)} rows read${listing.truncated ? " · the first 10,000 paths" : ""}`);
    if (!ps.length) { clear(list, empty("table", "No paths yet", `No row of ${table} holds a key in ${field}.`)); return; }
    const f = q.value.trim().toLowerCase();
    const out = [];
    const walk = (nodes, depth) => {
      for (const n of nodes) {
        // Filtered, the paths that match, each at its depth among them.
        const hit = !f || n.keys.join(".").toLowerCase().includes(f);
        if (hit) out.push(row(n, depth));
        if (n.kids.length && (f || !shut.has(n.keys.join("\u0000")))) walk(n.kids, hit ? depth + 1 : depth);
      }
    };
    walk(pathNodes(listing), 0);
    clear(list, out.length ? out : h("div", { class: "muted small", style: { padding: "8px 2px" } }, `No path has “${q.value}”.`));
  }
  function row(n, depth) {
    const p = n.p || { path: n.keys.join("."), keys: n.keys, rows: 0, types: {}, distinct: 0 };
    const id = n.keys.join("\u0000");
    const share = listing.rows ? p.rows / listing.rows : 0;
    const open = !shut.has(id);
    const tog = n.kids.length ? h("button", { class: "pt-tog", "aria-label": open ? "Fold" : "Unfold", onclick: (e) => { e.stopPropagation(); open ? shut.add(id) : shut.delete(id); draw(); } }, icon(open ? "down" : "right")) : h("span", { class: "pt-tog" });
    const r = h("div", { class: "pt-row" + (on === p.path ? " on" : ""), role: "treeitem", tabIndex: 0, "aria-expanded": n.kids.length ? String(open) : null, style: { "--d": depth }, title: p.path },
      h("span", { class: "pt-name" }, tog, h("span", { class: "pt-key mono ellipsis" }, n.key)),
      h("span", { class: "pt-share", title: `${fmt.si(p.rows)} of ${fmt.si(listing.rows)} rows` }, h("i", { style: { width: Math.max(share > 0 ? 2 : 0, Math.min(100, share * 100)) + "%" } })),
      h("span", { class: "small dim num pt-pct" }, share > 0 && share < 0.01 ? "<1%" : fmt.pct(share, 0)),
      typeMix(p),
      h("span", { class: "small dim ellipsis pt-sum" }, pathSummary(p, n.kids.length)));
    const pick = () => { on = on === p.path ? null : p.path; draw(); };
    r.onclick = pick;
    r.onkeydown = (e) => {
      if (e.key === "Enter" || e.key === " ") { e.preventDefault(); pick(); }
      else if (e.key === "ArrowRight" && n.kids.length && !open) { shut.delete(id); draw(); }
      else if (e.key === "ArrowLeft" && n.kids.length && open) { shut.add(id); draw(); }
    };
    return on === p.path && n.p ? [r, detail(p)] : r;
  }
  function detail(p) {
    const col = pathSql(field, p.keys);
    const t = quoteName(table);
    const facet = `SELECT ${col}, COUNT(*) AS n\nFROM ${t}\nGROUP BY ${col}\nORDER BY n DESC\nLIMIT 20;`;
    const rowsSql = `SELECT *\nFROM ${t}\nWHERE ${col} IS NOT NULL\nLIMIT 20;`;
    const vs = p.values || [];
    const top = vs.slice(0, 12);
    const most = Math.max(1, ...top.map((v) => v.rows));
    return h("div", { class: "pt-detail" },
      h("div", { class: "row", style: { gap: "6px" } },
        h("code", { class: "mono small" }, col),
        h("span", { class: "small dim" }, `${fmt.plural(p.rows, "row")}`, ty(p)),
        h("span", { class: "sep" }),
        h("button", { class: "btn sm ghost", title: "Copy how SQL names it", onclick: () => copy(col, "Path copied") }, icon("copy")),
        h("a", { class: "btn sm", href: "#/sql?q=" + encodeURIComponent(facet), title: "Count the rows of each value" }, icon("layers"), "Facet"),
        h("a", { class: "btn sm", href: "#/sql?q=" + encodeURIComponent(rowsSql), title: "The rows that hold it" }, icon("sql"), "Rows")),
      top.length ? h("div", { class: "pt-vals" }, top.map((v) => {
        const ex = `SELECT *\nFROM ${t}\nWHERE ${col} = ${valueSql(v.value)}\nLIMIT 20;`;
        return h("a", { class: "pt-val", href: "#/sql?q=" + encodeURIComponent(ex), title: `${fmt.plural(v.rows, "row")}: query them` },
          h("span", { class: "mono ellipsis" }, typeof v.value === "string" ? v.value : String(v.value)),
          h("span", { class: "pt-vbar" }, h("i", { style: { width: (v.rows / most * 100) + "%", background: PATH_TYPES.find(([k]) => k === (typeof v.value === "string" ? "string" : typeof v.value === "number" ? "number" : "bool"))[1] } })),
          h("span", { class: "small dim num" }, fmt.si(v.rows)));
      }), vs.length > top.length ? h("span", { class: "small muted" }, `and ${vs.length - top.length} more`) : null)
        : h("div", { class: "small muted" }, p.distinct == null || p.distinct > 100 ? "More than 100 values: facet it to count them." : "No values here: objects, or strings longer than 256 bytes."));
  }
  const ty = (p) => { const k = pathType(p); return k ? ` · mostly ${KIND[k]}s` : ""; };
  load(false);
  return box;
}
