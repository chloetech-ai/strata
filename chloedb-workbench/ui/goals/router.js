// The routes (JOURNEYS.md, "URL routes"): what page a hash names, and the
// old hashes as redirects. Pure: `resolve(hash, { project, agentIds })`
// gives `{ redirect }` or `{ page, ...params, query }`. `project` is the
// short name of the project in focus (the last one opened, or the most
// active), for the old routes that had none.

function split(hash) {
  const h = (hash || "").replace(/^#/, "");
  const qi = h.indexOf("?");
  const path = qi >= 0 ? h.slice(0, qi) : h;
  const query = new URLSearchParams(qi >= 0 ? h.slice(qi + 1) : "");
  return { parts: path.split("/").filter(Boolean).map((p) => { try { return decodeURIComponent(p); } catch { return p; } }), query, raw: qi >= 0 ? h.slice(qi + 1) : "" };
}

const MORE = ["terminals", "time", "decisions", "library", "rules", "guide", "keys", "setup", "settings"];

export function resolve(hash, { project = null, agentIds = [] } = {}) {
  const { parts, query, raw } = split(hash);
  const q = raw ? `?${raw}` : "";
  const p = project;
  const P = (rest = "") => (p ? `#/p/${encodeURIComponent(p)}${rest}` : "#/p");
  // #<agent id> (the old zoom)
  if (!hash || hash === "#" || hash === "#/") return { page: "start", query };
  if (!hash.startsWith("#/")) {
    const id = hash.slice(1);
    if (id === "decisions") return { redirect: "#/more/decisions" };
    if (agentIds.includes(id) || /^[0-9a-z]{4,12}$/.test(id)) return { redirect: `#/a/${id}` };
    return { redirect: "#/" };
  }
  const [a, b, c, d] = parts;
  switch (a) {
    case "new": return { page: "new", step: b || "folder", query };
    case "needs": return { page: "needs", query };
    case "p":
      if (!b) return { page: "open", query };
      if (!c) return { page: "continue", project: b, query };
      if (c === "understand") return { page: "understand", project: b, query };
      if (c === "change") return { page: "change", project: b, query };
      if (c === "ship") return { page: "ship", project: b, agent: d || null, query };
      return { redirect: `#/p/${encodeURIComponent(b)}` };
    case "a":
      if (!b) return { redirect: "#/" };
      return { page: "agent", agent: b, tab: ["changes", "preview", "terminal", "events"].includes(c) ? c : "progress", query };
    case "debug": return { page: "debug", rest: parts.slice(1), query };
    case "more":
      if (MORE.includes(b)) return { page: "more", item: b, query };
      return { page: "more", item: null, query };
    // Today's routes, for a release (JOURNEYS.md, "URL routes").
    case "mission": return { redirect: P("/change") };
    case "decide": case "away": case "catchup": return { redirect: P() };
    case "trace": case "why": return { redirect: `#/debug/code${q}` };
    case "map": return { redirect: P("/understand") };
    case "story": return { redirect: b ? `#/a/${b}` : P() };
    case "preview": return { redirect: b ? `#/a/${b}/preview` : P() };
    case "ship": return { redirect: b ? P(`/ship/${b}`) : P("/ship") };
    case "health": return { redirect: "#/debug/failing" };
    case "flow": return { redirect: "#/more/time" };
    case "library": return { redirect: `#/more/library${q}` };
    case "grid": return { redirect: "#/more/terminals" };
    case "guide": return { redirect: "#/more/guide" };
    case "zoom": return { redirect: b ? `#/a/${b}/terminal` : "#/" };
    default: return { redirect: "#/" };
  }
}
