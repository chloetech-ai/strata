// The database's tables, read once and shared by the views that write
// SQL (the workbench, the builder, the schema, imports): their fields in
// the short JSON form, the CREATE TABLE that makes each, and how the
// tables and fields point at one another.

import { h, api, bus } from "./core.js";

let cache = null, at = 0, pending = null;

/** Every table ([{table, short, sql, profiles, embeddings, derived, ...}]), at most 30 s old. */
export async function tables(force = false) {
  if (cache && !force && Date.now() - at < 30000) return cache;
  if (!pending) pending = api("/v1/tables").then((j) => { cache = j.tables; at = Date.now(); bus.emit("catalog", cache); return cache; }).finally(() => { pending = null; });
  return pending;
}
export const cached = () => cache || [];
export const invalidate = () => { at = 0; };
bus.on("token", () => { cache = null; at = 0; });

export const table = (name) => cached().find((t) => t.table.toLowerCase() === String(name).toLowerCase());

/** What each type is for, with a glyph and a color slot. */
export const TYPES = {
  text: { label: "TEXT", glyph: "T", color: "var(--s1)", what: "Words, searched with MATCH and ranked with bm25" },
  keyword: { label: "KEYWORD", glyph: "K", color: "var(--s3)", what: "Exact values: filters, facets, IN, prefixes" },
  int: { label: "INT", glyph: "#", color: "var(--s4)", what: "Whole numbers: ranges, sums, sorting" },
  float: { label: "FLOAT", glyph: "#", color: "var(--s4)", what: "Numbers: ranges, averages, boosts" },
  bool: { label: "BOOL", glyph: "✓", color: "var(--s7)", what: "True or false" },
  timestamp: { label: "TIMESTAMP", glyph: "◷", color: "var(--s7)", what: "A moment in UTC: ranges, decay by age" },
  vector: { label: "VECTOR", glyph: "⟂", color: "var(--s5)", what: "Embeddings: similarity, nearest rows" },
  sparse: { label: "SPARSE", glyph: "∴", color: "var(--s5)", what: "Learned term weights (SPLADE)" },
  media: { label: "MEDIA", glyph: "▣", color: "var(--s2)", what: "A file by reference: images, sound, video, PDF" },
  edges: { label: "EDGES", glyph: "↗", color: "var(--s8)", what: "Links to other rows: graph walks, PageRank" },
};
export const typeMeta = (f) => TYPES[f.type] || { label: String(f.type).toUpperCase(), glyph: "?", color: "var(--muted)", what: "" };

/** A small type badge for a field. */
export function typeBadge(f) {
  const m = typeMeta(f);
  return h("span", { class: "tb", style: { "--c": m.color }, title: `${m.label}${f.multi ? "[]" : ""}: ${m.what}` }, m.glyph);
}

/** Field types a filter can test, and its operators. */
export function operators(f) {
  if (f.type === "text") return [["matches", "has the words"]];
  if (f.type === "keyword" || f.type === "media") return f.multi ? [["=", "has"], ["!=", "has not"], ["in", "has one of"], ["prefix", "starts with"], ["null", "is empty"], ["notnull", "is set"]] : [["=", "is"], ["!=", "is not"], ["in", "is one of"], ["prefix", "starts with"], ["null", "is empty"], ["notnull", "is set"]];
  if (f.type === "int" || f.type === "float" || f.type === "timestamp") return [[">=", "≥"], ["<=", "≤"], [">", ">"], ["<", "<"], ["=", "="], ["!=", "≠"], ["between", "between"], ["null", "is empty"], ["notnull", "is set"]];
  if (f.type === "bool") return [["true", "is true"], ["false", "is false"], ["null", "is empty"]];
  return [];
}

/**
 * The links between tables and fields: references (a column holding
 * another table's keys), EDGES fields, edge tables, embedded vectors,
 * generated and derived text, semantic IDs.
 * [{kind, from: {table, field}, to: {table, field}, label}]
 */
export function links(ts = cached()) {
  const out = [];
  for (const t of ts) {
    const s = t.short || {};
    const key = (name) => table(name)?.short?.key || null;
    for (const f of s.fields || []) {
      if (f.references) out.push({ kind: f.type === "edges" ? "edges" : "ref", from: { table: s.table, field: f.name }, to: { table: f.references, field: key(f.references) }, label: f.type === "edges" ? "links to" : "references" });
      else if (f.type === "edges") out.push({ kind: "edges", from: { table: s.table, field: f.name }, to: { table: s.table, field: s.key }, label: "links to" });
      if (f.embed) for (const src of [].concat(f.embed.from)) out.push({ kind: "embed", from: { table: s.table, field: src }, to: { table: s.table, field: f.name }, label: f.embed.model });
      if (f.from) for (const src of [].concat(f.from)) out.push({ kind: "from", from: { table: s.table, field: src }, to: { table: s.table, field: f.name }, label: "joined into" });
      if (f.derive) out.push({ kind: "derive", from: { table: s.table, field: f.derive.from }, to: { table: s.table, field: f.name }, label: f.derive.model });
      if (f.semantic_id) out.push({ kind: "sid", from: { table: s.table, field: f.semantic_id.from }, to: { table: s.table, field: f.name }, label: f.semantic_id.codebook });
    }
    if (s.edges) {
      const [src, dst] = String(s.edges).split(",").map((x) => x.trim());
      const over = (s.fields || []).find((f) => f.name === src)?.references;
      if (over) out.push({ kind: "graph", from: { table: s.table, field: null }, to: { table: over, field: null }, label: `edges ${src} → ${dst}` });
    }
  }
  return out;
}

/** A SQL string literal. */
export const lit = (v) => `'${String(v).replace(/'/g, "''")}'`;
