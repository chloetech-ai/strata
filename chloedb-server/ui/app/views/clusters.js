// All clusters: the hub's own server and every connection in its book
// (chloedb-server's crate::hub) on one page. Each one's health, rows and
// key as it answers now, through the hub; and what differs between them
// that can be made the same: tables (their CREATE TABLE), synonyms and
// alert rules, compared with the one picked as the reference and applied
// to the others on request, production last. Everything goes through the
// hub's forward (/c/<name>/...), which adds each cluster's key; nothing is
// kept here.
//
// Which clusters follow which (chloedb-server's crate::follow): an arrow
// from each source to its follower, with how far behind it is, from each
// follower's follow view; "Follow…" sets one connection to follow another
// (the hub's POST /v1/admin/hub/follow), and the follower's Follow view
// pauses, stops and promotes it.

import { h, icon, clear, fmt, api, toast, confirm, card, empty, failure, pill, connectionName, every } from "../core.js";
import { startFollow, behindText } from "../follows.js";

export default {
  id: "clusters", title: "All clusters", icon: "cluster", group: "Change",
  keywords: "all clusters fleet many clusters hub overview health drift compare config settings synonyms alert rules tables schema apply sync revoke device",
  render,
};

/** What is compared: each kind's items by name, from what an end answers. */
const KINDS = [
  ["table", "Tables", "the CREATE TABLE that makes it"],
  ["synonyms", "Synonyms", "the synonym rules"],
  ["rule", "Alert rules", "the rule as set (not its state)"],
];

const isProd = (env) => !!env && /^prod/i.test(env);

function render(page) {
  if (connectionName) {
    page.append(card({ title: "All clusters", body: h("div", { class: "stack" }, h("p", { class: "dim", style: { margin: 0 } }, "This page is the hub's: it lists every cluster the hub reaches."), h("div", {}, h("a", { class: "btn sm", href: "/#/clusters" }, "Open it on the hub"))) }));
    return;
  }
  const tiles = h("div"), links = h("div"), drift = h("div");
  page.append(tiles, links, drift);
  let alive = true;
  let book = null;
  let ends = [];         // [{name, label, path, color, env, conn}]
  const seen = new Map(); // name -> {stats, cfg, error}
  const follows = new Map(); // name -> {view, spec} of its follow view, or null
  let base = "";

  async function load() {
    try { book = await api("/v1/admin/hub", { hub: true }); }
    catch (e) { clear(tiles, failure(e, "The hub's book")); return; }
    ends = [{ name: "", label: "this server", path: "", color: "var(--text-3)", env: null, conn: null },
      ...(book.connections || []).map((c) => ({ name: c.name, label: c.name, path: `/c/${c.name}`, color: c.color, env: c.environment || c.cluster?.environment, conn: c }))];
    drawTiles(); drawDrift();
    await Promise.all([...ends.map(ask), askFollows()]);
  }

  /** Each end's follow view, and its operator's spec.follow (to say "paused"). */
  async function askFollows() {
    await Promise.all(ends.map(async (e) => {
      const [view, kube] = await Promise.all([api(e.path + "/v1/admin/follow", { hub: true, timeout: 10000 }).catch(() => null), api(e.path + "/v1/kube", { hub: true, timeout: 10000 }).catch(() => null)]);
      follows.set(e.name, view ? { view, spec: kube?.spec?.follow || null } : null);
    }));
    if (!alive) return;
    drawTiles(); drawLinks();
  }

  /** The end that `e`'s follow process reads: by the name it follows, or the source's URL. */
  function sourceEnd(e, name, url) {
    const t = (u) => String(u || "").replace(/\/+$/, "");
    return ends.find((x) => x !== e && x.conn && (x.name === name || (url && [...(x.conn.urls || []), x.conn.cluster?.public_url].some((u) => u && t(u) === t(url))))) || null;
  }

  /** Who follows whom: [{src, srcLabel, dst, c, spec, tone, text}]. */
  function pairs() {
    const out = [];
    for (const e of ends) {
      const f = follows.get(e.name);
      const name = f?.view?.following;
      if (!name) continue;
      const c = (f.view.clusters || []).find((x) => x.name === name) || {};
      const st = c.status;
      const src = sourceEnd(e, name, st?.source);
      const paused = f.spec?.name === name && f.spec.paused;
      const silent = !st || c.status_age_ms > 30_000;
      const lag = st?.lag_ms ?? 0;
      const [tone, text] = paused ? ["warn", "paused"]
        : silent ? ["bad", st ? `silent ${fmt.ms(c.status_age_ms)}` : "no follow process"]
        : st.state === "held" ? ["bad", "held: " + (st.error || "?")]
        : st.state === "stopped" ? ["bad", "stopped"]
        : st.state === "copying" || st.state === "catching up" ? ["info", `${st.state}, ${fmt.n(st.rows_copied || 0)} rows`]
        : lag > 60_000 ? ["warn", behindText(lag)]
        : ["ok", behindText(lag)];
      out.push({ src, srcLabel: src?.label || name, dst: e, c, tone, text, behind: st?.behind });
    }
    return out;
  }

  /** An arrow from each source to its follower, with how far behind it is. */
  function drawLinks() {
    const ps = pairs();
    const chip = (e, label) => h("span", { class: "row nw follow-chip" }, e?.conn ? h("span", { class: "conn-dot", style: { background: e.color } }) : icon("db"), h("b", {}, label));
    clear(links, ps.length ? card({
      title: "Following", sub: `${fmt.plural(ps.length, "cluster")} follow${ps.length === 1 ? "s" : ""} another`,
      body: h("div", { class: "stack follow-links" }, ps.map((p) => h("div", { class: "follow-link " + p.tone },
        chip(p.src, p.srcLabel),
        h("span", { class: "follow-arrow", title: p.behind != null ? `${fmt.plural(p.behind, "transaction")} to apply` : "" }, h("span", { class: "follow-lag small" }, p.text)),
        chip(p.dst, p.dst.label),
        h("a", { class: "btn sm", href: p.dst.conn ? `/c/${p.dst.name}#/follow` : "/#/follow", title: "Pause, stop or promote it there" }, icon("share"), "Follow view")))),
    }) : null);
  }

  /** One end's health and what is compared, as it answers now. */
  async function ask(e) {
    const at = (p) => api(e.path + p, { hub: true, timeout: 15000 });
    const out = { at: Date.now() };
    try {
      const [stats, tables, syn, alerts] = await Promise.all([at("/v1/health"), at("/v1/tables"), at("/v1/synonyms").catch(() => null), at("/v1/alerts").catch(() => null)]);
      out.stats = stats;
      // LIV-13: the tables it lists, and the rows of them all: its health
      // counts them (a coordinator from this version on), else the sizes
      // it keeps of each table (`docs` counts its first table only).
      out.tableCount = (tables.tables || []).length;
      out.rows = stats.rows ?? null;
      if (out.rows == null) { const ix = await at("/v1/index/tables").catch(() => null); if (ix?.tables) out.rows = ix.tables.reduce((a, t) => a + (t.docs || 0), 0); }
      const items = {};
      for (const t of tables.tables || []) items[`table:${t.table}`] = { kind: "table", name: t.table, value: t.sql, shard_by: t.shard_by };
      if (syn) items["synonyms:"] = { kind: "synonyms", name: "rules", value: (syn.text || "").trim(), rules: syn.rules, raw: syn.text || "" };
      for (const r of alerts?.rules || []) { const { status, ...rule } = r; items[`rule:${r.id}`] = { kind: "rule", name: r.name || r.id, value: JSON.stringify(rule, Object.keys(rule).sort()), rule }; }
      out.items = items;
    } catch (err) { out.error = err.message; }
    seen.set(e.name, out);
    if (!alive) return;
    drawTiles(); drawDrift();
  }

  function drawTiles() {
    const box = h("div", { class: "fleet-tiles" }, ends.map((e) => {
      const s = seen.get(e.name);
      const st = s?.stats;
      const status = !s ? pill("info", "asking…") : s.error ? pill("bad", "not answering") : st?.status === "ok" ? pill("ok", "ok") : pill("warn", st?.status || "?");
      const key = e.conn?.key || {};
      const expires = e.conn ? (e.conn.keyless ? "through its Kubernetes API" : key.expires_ms ? `key expires ${fmt.ago(key.expires_ms).replace(" ago", "")} from now` : `${key.scope || "?"} key, no expiry`) : "the hub itself";
      const f = follows.get(e.name)?.view;
      const p = f?.following ? pairs().find((x) => x.dst === e) : null;
      const fed = pairs().filter((x) => x.src === e);
      return h("div", { class: "fleet-tile", style: { "--c": e.color } },
        h("div", { class: "row between nw" }, h("span", { class: "row nw", style: { gap: "6px" } }, e.conn ? h("span", { class: "conn-dot", style: { background: e.color } }) : icon("db"), h("b", {}, e.label), isProd(e.env) ? h("span", { class: "badge" }, "PROD") : null), status),
        h("div", { class: "small dim" }, [e.conn?.cluster?.name, e.env].filter(Boolean).join(" · ") || (e.conn ? "unnamed cluster" : location.host)),
        s?.error ? h("div", { class: "small error" }, s.error)
          : st ? h("div", { class: "fleet-nums" },
            h("div", { title: s.rows == null ? "This server does not count the rows of every table" : "" }, h("b", { class: "num" }, s.rows == null ? "–" : fmt.n(s.rows)), h("span", { class: "small dim" }, " rows")),
            h("div", {}, h("b", { class: "num" }, fmt.n(s.tableCount)), h("span", { class: "small dim" }, s.tableCount === 1 ? " table" : " tables")),
            h("div", {}, h("span", { class: "small dim" }, st.role === "coordinator" ? "a cluster" : "one server")))
          : null,
        p ? h("div", { class: "row nw small", style: { gap: "6px" } }, icon("share"), "follows ", h("b", {}, p.srcLabel), pill(p.tone, p.text))
          : fed.length ? h("div", { class: "row nw small dim", style: { gap: "6px" } }, icon("share"), "followed by ", fed.map((x) => x.dst.label).join(", ")) : null,
        h("div", { class: "row between nw small" }, h("span", { class: "dim" }, expires),
          h("span", { class: "row nw", style: { gap: "6px" } },
            e.conn && (book?.connections || []).length > 1 ? h("button", { class: "btn sm ghost", title: `Set ${e.label} to follow another cluster`, onclick: () => follow({ follower: e.name }) }, icon("share"), "Follow…") : null,
            h("a", { class: "btn sm", href: e.conn ? `/c/${e.name}#/home` : "/#/home" }, "Open"))));
    }));
    clear(tiles, card({
      title: "All clusters", sub: `this server and ${fmt.plural((book?.connections || []).length, "connection")}`, cls: "fleet-card",
      tools: [h("button", { class: "btn sm ghost", title: "Ask them all again", onclick: () => { seen.clear(); drawTiles(); drawDrift(); ends.forEach(ask); askFollows(); } }, icon("refresh")),
        (book?.connections || []).length > 1 ? h("button", { class: "btn sm", title: "Set one cluster to follow another: it copies it, then applies its changes", onclick: () => follow({}) }, icon("share"), "Follow…") : null,
        h("a", { class: "btn sm", href: "/#/connections" }, icon("key"), "Connections"),
        (book?.connections || []).length ? h("button", { class: "btn sm danger", title: "Revokes the key this hub holds on each cluster, there, and takes each out of the book: for a device that is leaving", onclick: revokeAll }, icon("trash"), "Revoke this hub's keys…") : null],
      body: (book?.connections || []).length ? box : h("div", { class: "stack" }, box, empty("cluster", "No other clusters yet", "Add one in Connections, or open one made from Cloud setup: it then shows here.")),
    }));
  }

  /** Each item, and where it is the same as the reference, differs, or is missing. */
  function drawDrift() {
    const others = ends.filter((e) => e.name !== base);
    const ref = seen.get(base);
    if (ends.length < 2) { clear(drift); return; }
    const sel = h("select", { class: "select sm", "aria-label": "Compared with", onchange: () => { base = sel.value; drawDrift(); } }, ends.map((e) => h("option", { value: e.name, selected: e.name === base }, e.label)));
    const head = h("div", { class: "row", style: { gap: "8px" } }, h("span", { class: "small dim" }, "Compared with"), sel);
    if (!ref || (!ref.items && !ref.error) || others.some((e) => !seen.get(e.name))) { clear(drift, card({ title: "What differs", tools: [head], body: h("div", { class: "small dim" }, "Asking each cluster for its tables, synonyms and alert rules…") })); return; }
    if (ref.error) { clear(drift, card({ title: "What differs", tools: [head], body: h("div", { class: "error small" }, `${ends.find((e) => e.name === base)?.label}: ${ref.error}`) })); return; }
    const keys = new Set(Object.keys(ref.items));
    for (const e of others) for (const k of Object.keys(seen.get(e.name)?.items || {})) keys.add(k);
    const order = (k) => KINDS.findIndex(([kind]) => k.startsWith(kind + ":"));
    const rows = [...keys].sort((a, b) => order(a) - order(b) || a.localeCompare(b)).map((k) => {
      const mine = ref.items[k];
      const any = mine || others.map((e) => seen.get(e.name)?.items?.[k]).find(Boolean);
      const cells = others.map((e) => {
        const s = seen.get(e.name);
        if (s.error) return { e, state: "unknown" };
        const theirs = s.items[k];
        if (!mine) return { e, state: theirs ? "only" : "absent" };
        if (!theirs) return { e, state: "missing" };
        return { e, state: theirs.value === mine.value ? "same" : "differs", theirs };
      });
      const fix = mine ? cells.filter((c) => c.state === "missing" || (c.state === "differs" && any.kind !== "table")) : [];
      return { k, item: any, mine, cells, fix };
    });
    const differ = rows.filter((r) => r.cells.some((c) => c.state !== "same" && c.state !== "absent"));
    const label = { same: ["ok", "same"], differs: ["warn", "differs"], missing: ["bad", "missing"], only: ["info", "only there"], absent: ["disabled", "–"], unknown: ["disabled", "?"] };
    const table = h("div", { class: "tbl-wrap" }, h("table", { class: "t" },
      h("thead", {}, h("tr", {}, h("th", {}, "what"), others.map((e) => h("th", {}, h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "conn-dot", style: { background: e.color } }), e.label, isProd(e.env) ? h("span", { class: "badge" }, "PROD") : null))), h("th", {}, ""))),
      h("tbody", {}, differ.map((r) => h("tr", {},
        h("td", {}, h("div", { class: "stack", style: { gap: "2px" } }, h("span", {}, h("span", { class: "small dim" }, KINDS.find(([k]) => k === r.item.kind)[1].replace(/s$/, ""), " "), h("b", { class: r.item.kind === "table" ? "mono" : "" }, r.item.name)),
          r.item.kind === "table" && r.mine?.shard_by ? h("span", { class: "small dim" }, `shard key ${r.mine.shard_by}`) : null)),
        r.cells.map((c) => h("td", { title: c.state === "differs" && r.item.kind === "table" ? "Its CREATE TABLE differs: change it there (ALTER TABLE), a copy does not" : "" }, pill(...label[c.state]))),
        h("td", {}, r.fix.length ? h("button", { class: "btn sm", onclick: () => apply(r) }, `Apply to ${fmt.plural(r.fix.length, "cluster")}…`) : null))))));
    const refLabel = ends.find((e) => e.name === base)?.label;
    clear(drift, card({
      title: "What differs", sub: differ.length ? `${fmt.plural(differ.length, "item")} not the same as on ${refLabel}` : `the same as on ${refLabel}`, tools: [head], flush: true,
      body: h("div", { class: "stack" }, differ.length ? table : h("div", { class: "small", style: { padding: "12px 12px 0" } }, icon("check"), rows.length ? ` The same everywhere: ${KINDS.map(([k, l]) => [rows.filter((r) => r.item.kind === k).length, l.toLowerCase()]).filter(([n]) => n).map(([n, l]) => (l === "synonyms" ? "the synonyms" : `${n} ${n === 1 ? l.replace(/s$/, "") : l}`)).join(", ")}.` : " Nothing to compare."),
        h("div", { class: "small dim", style: { padding: "0 12px 12px" } }, `Tables are compared by their CREATE TABLE, synonyms by their rules, alert rules as set. Applying makes a missing table (with no rows: copy them from Connections), replaces the synonyms, or sets the rule as on ${refLabel}; production goes last.`)),
    }));
  }

  /** Makes `r` on the clusters where it differs or is missing as it is on the reference, production last. */
  async function apply(r) {
    const targets = r.fix.map((c) => c.e).sort((a, b) => isProd(a.env) - isProd(b.env));
    const what = r.item.kind === "table" ? `the table ${r.item.name} (no rows)` : r.item.kind === "synonyms" ? "the synonym rules" : `the alert rule "${r.item.name}"`;
    const ok = await confirm({
      title: `Apply ${what}?`, ok: "Apply", danger: targets.some((e) => isProd(e.env)),
      body: h("div", { class: "stack" },
        h("p", { class: "dim", style: { margin: 0 } }, `As on ${ends.find((e) => e.name === base)?.label}, to:`),
        h("ol", { style: { margin: 0 } }, targets.map((e) => h("li", {}, e.label, isProd(e.env) ? h("span", { class: "badge", style: { marginLeft: "6px" } }, "PROD") : null))),
        r.item.kind === "synonyms" ? h("p", { class: "dim small", style: { margin: 0 } }, "Their synonym rules are replaced by these.") : null),
    });
    if (!ok) return;
    for (const e of targets) {
      try {
        if (r.item.kind === "table") await api(`${e.path}/v1/sql`, { body: { sql: r.mine.value }, hub: true, reason: `the same table as on ${base || "the hub"}` });
        else if (r.item.kind === "synonyms") await api(`${e.path}/v1/synonyms`, { method: "PUT", body: { text: r.mine.raw }, hub: true });
        else {
          const theirs = await api(`${e.path}/v1/alerts`, { hub: true });
          const rules = (theirs.rules || []).map(({ status, ...x }) => x).filter((x) => x.id !== r.mine.rule.id);
          await api(`${e.path}/v1/alerts/rules`, { method: "PUT", body: { rules: [...rules, r.mine.rule] }, hub: true });
        }
        toast(`${e.label}: ${what} applied`, "ok");
      } catch (err) { toast(`${e.label}: ${err.message}`, "bad", 9000); break; }
    }
    for (const e of targets) ask(e);
  }

  /** Revokes, on each cluster, the key this hub holds there, then takes the connection out of the book. */
  async function revokeAll() {
    const cs = book?.connections || [];
    const ok = await confirm({
      title: `Revoke this hub's keys on ${fmt.plural(cs.length, "cluster")}?`, ok: "Revoke them", danger: true,
      body: h("div", { class: "stack" },
        h("p", { class: "dim", style: { margin: 0 } }, `Each cluster stops taking the key this hub (${book.device}) holds, and the connection leaves the book. Other keys and devices keep working. A key that is the cluster's token is only forgotten here: revoke the token on the cluster.`),
        h("ul", { style: { margin: 0 } }, cs.map((c) => h("li", {}, c.name, " · ", c.key?.root ? "the cluster's token" : `${c.key?.scope || "?"} key ${c.key?.name || c.key?.id || ""}`)))),
    });
    if (!ok) return;
    for (const c of cs) {
      try {
        if (c.key?.id && !c.key.root) await api(`/c/${c.name}/v1/admin/keys/${encodeURIComponent(c.key.id)}`, { method: "DELETE", hub: true, reason: `device ${book.device} leaving` }).catch((e) => { if (!/refused this hub's key|502/.test(e.message)) throw e; });
        await api(`/v1/admin/hub/connections/${c.name}`, { method: "DELETE", hub: true });
        toast(`${c.name}: key revoked, out of the book`, "ok");
      } catch (e) { toast(`${c.name}: ${e.message}`, "bad", 9000); }
    }
    seen.clear(); load();
  }

  /** Sets a connection to follow another (the Start card), then asks again. */
  async function follow(ask) {
    const r = await startFollow(book, ask);
    if (r) askFollows();
  }

  load();
  const stopFollows = every(5000, () => (ends.length ? askFollows() : null), false);
  return () => { alive = false; stopFollows(); };
}
