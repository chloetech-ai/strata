// The crawler's page in ChloeDB's command center (#/app/crawler). The
// command center loads this file through chloedb-server, which forwards
// /v1/apps/crawler/... to the crawler, and runs it with its own helpers:
// render(el, ctx) gets ctx.core (h, card, table, fmt, ...), ctx.charts,
// ctx.api (the crawler's API) and ctx.core.api (the server's: the table's
// rows). It imports nothing, so the crawler serves it as one file.
//
//   #/app/crawler        a new crawl (preview, then start), the crawls, the cluster
//   #/app/crawler/<id>   one crawl: its rate, the table's schema as it grows,
//                        the shards filling and splitting, its pages and problems

export default { render };

const CSS = `
.cr-hero { display: grid; grid-template-columns: minmax(0, 1fr) auto auto; gap: 8px; }
.cr-hero .input { font-size: 15px; padding: 9px 12px; }
.cr-opts { display: grid; grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 10px 14px; margin-top: 12px; }
.cr-opts .wide { grid-column: span 2; }
.cr-opts textarea.input { font-family: var(--mono); font-size: 12.5px; min-height: 58px; resize: vertical; }
.cr-steps { display: flex; gap: 4px 14px; flex-wrap: wrap; font-size: 12px; color: var(--muted); margin-top: 12px; }
.cr-steps span b { color: var(--text-2); font-weight: 600; margin-right: 4px; }
.cr-shard { display: grid; grid-template-columns: 70px minmax(0, 1fr) minmax(90px, auto); gap: 12px; align-items: center; padding: 7px 0; }
.cr-meter { position: relative; height: 12px; background: var(--surface-3); border-radius: 6px; }
.cr-meter > i { position: absolute; left: 0; top: 0; bottom: 0; border-radius: 6px; background: var(--s1); transition: width .5s; }
.cr-meter > i.over { background: var(--s2); }
.cr-meter > u { position: absolute; top: -4px; bottom: -4px; width: 2px; margin-left: -1px; background: var(--text-2); border-radius: 1px; }
.cr-list { list-style: none; margin: 0; padding: 0; }
.cr-list li { display: grid; grid-template-columns: 62px 86px minmax(0, 1fr); gap: 10px; padding: 7px 0; border-top: 1px solid var(--line); font-size: 13px; }
.cr-list li:first-child { border-top: 0; }
.cr-list pre.code { margin-top: 6px; white-space: pre-wrap; overflow-wrap: anywhere; }
.cr-bar { height: 6px; background: var(--surface-3); border-radius: 3px; overflow: hidden; margin: 0 0 14px; }
.cr-bar > i { display: block; height: 100%; background: var(--accent); transition: width .5s; }
.cr-url { display: block; max-width: 320px; }
.cr-sub { margin: 14px 0 6px; font-size: 12px; font-weight: 600; color: var(--text-2); text-transform: uppercase; letter-spacing: .04em; }
.cr-sub:first-child { margin-top: 0; }
.cr-search { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 8px; margin-bottom: 10px; }
@media (max-width: 900px) {
  .cr-hero, .cr-search { grid-template-columns: minmax(0, 1fr); }
  .cr-opts .wide { grid-column: auto; }
  .cr-list li { grid-template-columns: 56px minmax(0, 1fr); }
  .cr-list li > :nth-child(2) { display: none; }
  .cr-url { max-width: 240px; }
}`;

const PHASE = { sampling: ["info", "reading a sample"], crawling: ["ok", "crawling"], paused: ["warn", "paused"], stopping: ["pending", "stopping"], done: ["good", "done"], stopped: ["", "stopped"], failed: ["bad", "failed"] };
const ACTIVE = new Set(["sampling", "crawling", "paused", "stopping"]);
const quote = (n) => `"${String(n).replace(/"/g, '""')}"`;

function render(el, ctx) {
  const { h } = ctx.core;
  el.append(h("style", {}, CSS));
  const root = h("div");
  el.append(root);
  let stop = null;
  const show = (path) => {
    try { stop?.(); } catch (e) { console.warn(e); }
    root.replaceChildren();
    const main = el.closest("main");
    if (main) main.scrollTop = 0;
    stop = path[0] ? crawlPage(root, ctx, path[0]) : homePage(root, ctx);
  };
  ctx.onPath(show);
  show(ctx.path());
  return () => stop?.();
}

// ---------------------------------------------------------------- shared bits

function phasePill(core, phase) {
  const [state, text] = PHASE[phase] || ["", phase || "?"];
  return core.pill(state || "info", text);
}

/** The cluster's shards, each a bar of its rows of the crawls' tables against the split threshold. */
function shardBars(core, grow) {
  const { h, fmt, empty } = core;
  const shards = grow?.shards || [];
  const split = grow?.policy?.split_rows || 0;
  if (!shards.length) {
    return empty("shard", grow?.role === "server" ? "One server, no shards" : "No shards yet",
      grow?.error ? grow.error : grow?.role === "server" ? "The crawler writes to a server alone: it has no shards to split. Run it against a coordinator, or with --local, to see the table spread." : "They show once the crawler has looked at the cluster.");
  }
  const most = Math.max(split, ...shards.map((s) => s.rows), 1) * 1.08;
  return h("div", {},
    shards.map((s) => h("div", { class: "cr-shard" },
      h("div", {}, h("b", {}, "shard " + s.shard), h("div", { class: "tiny muted" }, fmt.plural(s.slots, "slot"))),
      h("div", { class: "cr-meter", title: `${fmt.n(s.rows)} rows of the crawls' tables (${fmt.n(s.all_rows)} of every table) on ${s.servers.join(", ")}` },
        h("i", { class: split && s.rows > split ? "over" : "", style: { width: Math.min(100, (s.rows / most) * 100) + "%", background: s.up ? null : "var(--crit)" } }),
        split ? h("u", { style: { left: (split / most) * 100 + "%" }, title: `splits at ${fmt.n(split)} rows` }) : null),
      h("div", { class: "num small" }, h("b", {}, fmt.n(s.rows)), " rows", s.up ? null : h("div", { class: "tiny", style: { color: "var(--crit-ink)" } }, "not answering")))),
    h("div", { class: "row small muted", style: { marginTop: "6px" } },
      split ? h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { style: { width: "2px", height: "12px", background: "var(--text-2)", display: "inline-block" } }), `a shard past ${fmt.n(split)} rows adds one`) : "growth is off",
      grow.moving ? core.pill("pending", grow.moves?.of ? `moving slots ${grow.moves.done ?? 0}/${grow.moves.of}` : "moving slots") : null,
      grow.waiting ? h("span", {}, "· " + grow.waiting) : null));
}

function growPolicy(core, grow) {
  const p = grow?.policy;
  if (!p) return null;
  const how = { off: "never grows", local: "grows with servers it starts here", servers: `grows onto ${p.servers?.length || 0} named servers`, kube: "grows through the Kubernetes operator" }[p.how] || p.how;
  return p.how === "off" || !p.split_rows ? how : `${how}: a shard per ${core.fmt.si(p.split_rows)} rows, up to ${p.max_shards}`;
}

function eventList(core, events, { max = 60 } = {}) {
  const { h, fmt, sqlBlock, empty } = core;
  if (!events.length) return empty("timeline", "Nothing yet");
  return h("ul", { class: "cr-list" }, events.slice(-max).reverse().map((e) => h("li", {},
    h("span", { class: "small muted num", title: fmt.datetime(e.at_ms) }, fmt.time(e.at_ms)),
    h("span", {}, h("span", { class: "badge" + (e.kind === "shard" || e.kind === "column" || e.kind === "fill" || e.kind === "schema" ? " accent" : "") }, e.kind)),
    h("div", {}, e.text, e.sql ? sqlBlock(e.sql) : null))));
}

/** Rates a second from a series of totals, over `w` samples. */
function rates(series, key, w = 5) {
  const out = [];
  for (let i = 1; i < series.length; i++) {
    const a = series[Math.max(0, i - w)], b = series[i];
    const dt = (b.t_ms - a.t_ms) / 1000;
    out.push([b.t_ms, dt > 0 ? (b[key] - a[key]) / dt : 0]);
  }
  return out;
}

function kpi(core, label) {
  const { h } = core;
  const val = h("div", { class: "val" }, "–"), delta = h("div", { class: "delta", style: { maxWidth: "100%" } });
  const el = h("div", { class: "card kpi" }, h("div", { class: "lbl" }, label), val, delta);
  let spark = null;
  return {
    el,
    set(v, d, values) {
      val.textContent = v;
      delta.textContent = d || "";
      delta.title = d || "";
      if (values && values.length > 1) { const s = core.charts.sparkline(values, { color: "var(--s1)" }); spark ? spark.replaceWith(s) : el.append(s); spark = s; delta.style.maxWidth = ""; }
    },
  };
}

// ---------------------------------------------------------------- a new crawl, the crawls, the cluster

function homePage(root, ctx) {
  const core = { ...ctx.core, charts: ctx.charts };
  const { h, icon, card, table, fmt, empty, failure, toast, every } = core;
  let info = null, gone = false;

  const conn = h("div", { class: "row" });
  root.append(h("div", { class: "page-head" },
    h("div", {}, h("h1", {}, "Crawler"), h("p", {}, "Give it a site: it reads the pages, finds their fields, makes a table from them and fills it, and adds shards as the table grows.")),
    h("div", { class: "tools" }, conn)));

  // The form.
  const f = {
    url: h("input", { class: "input", type: "url", placeholder: "https://books.toscrape.com/", "aria-label": "The site's address", autocomplete: "url", spellcheck: "false" }),
    table: h("input", { class: "input", placeholder: "from the address" }),
    scope: h("select", { class: "select" }, [["host", "this host"], ["domain", "the whole site"], ["prefix", "under this path"], ["any", "anywhere"]].map(([v, t]) => h("option", { value: v }, t))),
    max_pages: h("input", { class: "input", type: "number", min: 1 }),
    max_depth: h("input", { class: "input", type: "number", min: 0 }),
    concurrency: h("input", { class: "input", type: "number", min: 1, max: 1024 }),
    per_host: h("input", { class: "input", type: "number", min: 1 }),
    delay_ms: h("input", { class: "input", type: "number", min: 0, step: 50 }),
    sample: h("input", { class: "input", type: "number", min: 1 }),
    promote_at: h("input", { class: "input", type: "number", min: 0, max: 1, step: 0.05 }),
    embed: h("input", { class: "input", placeholder: "none (as potion-base-8M)" }),
    contact: h("input", { class: "input", placeholder: "an email or a URL", spellcheck: "false" }),
    include: h("textarea", { class: "input", placeholder: "/catalogue/", spellcheck: "false" }),
    exclude: h("textarea", { class: "input", placeholder: "\\?sort=", spellcheck: "false" }),
    keep: h("textarea", { class: "input", placeholder: "/catalogue/[^/]+_\\d+/", spellcheck: "false" }),
    rules: h("textarea", { class: "input", placeholder: "price=p.price_color\nimage=#product_gallery img@src\ntags[]=.tags a", spellcheck: "false" }),
  };
  const checks = { robots: true, sitemaps: true, learn: true };
  const toggles = {
    robots: core.toggle(true, (v) => { checks.robots = v; }, "obey robots.txt"),
    sitemaps: core.toggle(true, (v) => { checks.sitemaps = v; }, "read sitemaps"),
    learn: core.toggle(true, (v) => { checks.learn = v; }, "learn the site's template"),
  };
  const field = (label, input, cls = "", title) => h("label", { class: "f " + cls, title }, label, input);
  // What the pace means for one site: the defaults are the crawler's promise
  // to the sites it reads (a request at a time, a second apart).
  const pace = h("p", { class: "small muted wide", "data-pace": "" });
  const showPace = () => {
    const pages = num(f.max_pages) || 1000, per = Math.max(1, num(f.per_host) || 1), delay = num(f.delay_ms) ?? 1000;
    const rate = delay > 0 ? (1000 / delay) * per : null;
    pace.textContent = rate ? `One site at this pace: about ${rate >= 10 || Number.isInteger(rate) ? Math.round(rate) : rate.toFixed(rate < 1 ? 2 : 1)} ${rate === 1 ? "page" : "pages"} a second, so ${pages.toLocaleString()} pages take about ${fmt.dur(pages / rate)}. Keep it for sites you don't run; a site of yours can take more requests at once and a shorter wait. List pages ("next") do not count toward the depth.` : "No wait between requests: only for a site you run.";
  };
  for (const k of ["max_pages", "per_host", "delay_ms"]) f[k].addEventListener("input", showPace);
  const previewBtn = h("button", { class: "btn", type: "button", onclick: () => preview() }, icon("eye"), "Preview");
  const startBtn = h("button", { class: "btn primary", type: "submit" }, icon("play"), "Crawl");
  const form = h("form", { onsubmit: (e) => { e.preventDefault(); start(); } },
    h("div", { class: "cr-hero" }, f.url, previewBtn, startBtn),
    h("details", { class: "more" }, h("summary", {}, "Options"),
      h("div", { class: "cr-opts" },
        field("Table", f.table, "", "The table it fills: made from the first pages, or an existing one it adds columns to"),
        field("Follow links", f.scope),
        field("At most pages", f.max_pages),
        field("At most links deep", f.max_depth),
        field("Requests at once", f.concurrency),
        field("Requests at once per host", f.per_host),
        field("Between requests to a host (ms)", f.delay_ms),
        pace,
        field("Pages the table is made from", f.sample),
        field("A column once this share of pages has the field", f.promote_at),
        field("Vectors of title and body, by the model", f.embed),
        field("Contact for site owners", f.contact, "", "Sent in the user agent, so that a site's owner can reach whoever runs this crawl"),
        h("div", { class: "stack wide", style: { gap: "8px", justifyContent: "flex-end" } }, toggles.robots, toggles.sitemaps, toggles.learn),
        field("Only links matching (one regex a line)", f.include, "wide"),
        field("Never links matching", f.exclude, "wide"),
        field("Rows only for pages matching (others only followed)", f.keep, "wide"),
        field("Fields from CSS selectors (name=selector, @attr, name[]= for all)", f.rules, "wide"))),
    h("div", { class: "cr-steps" },
      h("span", {}, h("b", {}, "1"), "fetches politely: robots.txt, a request at a time per host, waits when a site is busy"),
      h("span", {}, h("b", {}, "2"), "reads each page's fields: JSON-LD, Open Graph, microdata, spec tables, the site's template"),
      h("span", {}, h("b", {}, "3"), "makes the table from the first pages"),
      h("span", {}, h("b", {}, "4"), "writes rows in batches, adding columns as fields become common"),
      h("span", {}, h("b", {}, "5"), "adds a shard when one passes its rows")));
  const previewBox = h("div");
  root.append(card({ title: "New crawl", body: form }), previewBox);

  const fill = (d) => {
    for (const k of ["max_pages", "max_depth", "concurrency", "per_host", "delay_ms", "sample", "promote_at"]) if (d[k] != null && !f[k].value) f[k].value = d[k];
    if (d.contact) f.contact.placeholder = d.contact;
    showPace();
  };
  const lines = (t) => t.value.split("\n").map((x) => x.trim()).filter(Boolean);
  const num = (inp) => (inp.value === "" ? undefined : Number(inp.value));
  function spec() {
    const url = f.url.value.trim();
    if (!url) { f.url.focus(); throw new Error("Give it the address of a page to start from"); }
    const s = {
      seeds: url.split(/\s+/).filter(Boolean), scope: f.scope.value,
      include: lines(f.include), exclude: lines(f.exclude), keep: lines(f.keep), rules: lines(f.rules),
      robots: checks.robots, sitemaps: checks.sitemaps, learn: checks.learn,
    };
    if (f.table.value.trim()) s.table = f.table.value.trim();
    if (f.embed.value.trim()) s.embed = f.embed.value.trim();
    if (f.contact.value.trim()) s.contact = f.contact.value.trim();
    for (const k of ["max_pages", "max_depth", "concurrency", "per_host", "delay_ms", "sample", "promote_at"]) { const v = num(f[k]); if (v != null && !Number.isNaN(v)) s[k] = v; }
    return s;
  }

  let asking = null;
  async function preview() {
    let s;
    try { s = spec(); } catch (e) { toast(e.message, "warn"); return; }
    asking?.abort();
    const ac = asking = new AbortController();
    previewBtn.disabled = true;
    previewBox.replaceChildren(card({ title: "Preview", body: h("div", { class: "row muted" }, icon("clock"), `Reading ${s.seeds[0]} and a few of the pages it links to…`) }));
    try {
      const p = await ctx.api("/v1/preview", { body: { ...s, pages: 10 }, timeout: 90000, signal: ac.signal });
      if (!gone && ac === asking) previewBox.replaceChildren(previewCard(core, p, () => start()));
    } catch (e) {
      if (!gone && ac === asking && e.name !== "AbortError") previewBox.replaceChildren(card({ title: "Preview", body: h("div", { class: "error" }, e.message) }));
    } finally { if (ac === asking) previewBtn.disabled = false; }
  }
  async function start() {
    let s;
    try { s = spec(); } catch (e) { toast(e.message, "warn"); return; }
    startBtn.disabled = true;
    try {
      const j = await ctx.api("/v1/crawls", { body: s });
      toast(`Crawling into ${j.table}`, "ok");
      location.hash = ctx.href(j.id);
    } catch (e) { toast(e.message, "bad", 8000); } finally { startBtn.disabled = false; }
  }

  // The crawls and the cluster.
  const crawls = table([
    { key: "table", label: "Table", render: (c) => h("div", {}, h("a", { href: ctx.href(c.id) }, h("b", {}, c.table)), h("div", { class: "tiny muted ellipsis cr-url", title: c.seeds.join(" ") }, c.seeds.join(" "))) },
    { key: "phase", label: "", render: (c) => phasePill(core, c.phase) },
    { key: "fetched", label: "Pages", num: true, render: (c) => fmt.n(c.fetched) },
    { key: "written", label: "Rows", num: true, render: (c) => fmt.n(c.written) },
    { key: "queued", label: "Queued", num: true, render: (c) => fmt.n(c.queued) },
    { key: "rate", label: "Pages/s", num: true, sort: (c) => c.rate?.fetched, render: (c) => (ACTIVE.has(c.phase) ? fmt.si(c.rate?.fetched || 0) : "–") },
    { key: "started_ms", label: "Started", num: true, render: (c) => h("span", { title: fmt.datetime(c.started_ms) }, fmt.ago(c.started_ms)) },
  ], { rows: [], empty: empty("route", "No crawls yet", "Start one above: the table and its shards show here as it fills."), onRow: (c) => { location.hash = ctx.href(c.id); } });
  const cluster = h("div");
  const growEvents = h("div");
  const policy = h("span", { class: "sub" });
  root.append(h("div", { class: "grid g2", style: { marginTop: "14px" } },
    card({ title: "Crawls", body: crawls, flush: true }),
    h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "The cluster"), policy), h("div", { class: "bd" }, cluster, h("div", { class: "cr-sub", style: { marginTop: "16px" } }, "How it grew"), growEvents))));

  async function loadInfo() {
    try {
      info = await ctx.api("/v1/info", { timeout: 10000 });
      if (gone) return;
      fill(info.defaults || {});
      const st = info.strata || {};
      conn.replaceChildren(
        st.error ? core.pill("bad", "ChloeDB does not answer") : core.pill("ok", st.role === "coordinator" ? "writes to a coordinator" : "writes to a server"),
        h("span", { class: "small muted", title: st.error || "" }, st.url, info.local ? " · started by the crawler" : ""));
    } catch (e) { if (!gone) conn.replaceChildren(core.pill("bad", "the crawler does not answer"), h("span", { class: "small muted" }, e.message)); }
  }
  let lastEvents = -1;
  async function tick() {
    const [c, g] = await Promise.allSettled([ctx.api("/v1/crawls", { timeout: 8000 }), ctx.api("/v1/grow", { timeout: 8000 })]);
    if (gone) return;
    if (c.status === "fulfilled") crawls.update(c.value.crawls || []);
    else crawls.update([]);
    if (g.status === "fulfilled") {
      const grow = g.value;
      policy.textContent = growPolicy(core, grow) || "";
      cluster.replaceChildren(shardBars(core, grow));
      if ((grow.events || []).length !== lastEvents) {
        lastEvents = (grow.events || []).length;
        growEvents.replaceChildren(lastEvents ? eventList(core, grow.events, { max: 12 }) : h("div", { class: "small muted" }, "Not yet: it adds a shard when one passes its rows."));
      }
    } else cluster.replaceChildren(failure(g.reason, "The cluster"));
  }
  loadInfo();
  const stopTick = every(2000, tick);
  const stopInfo = every(15000, loadInfo, false);
  setTimeout(() => f.url.focus(), 50);
  return () => { gone = true; asking?.abort(); stopTick(); stopInfo(); };
}

function previewCard(core, p, start) {
  const { h, icon, card, table, fmt, sqlBlock, shareBar } = core;
  const cols = p.plan?.columns || [];
  const pages = table([
    { key: "url", label: "Page", render: (r) => h("div", {}, h("div", { class: "ellipsis cr-url" }, r.title || "(no title)"), h("a", { class: "tiny ellipsis cr-url", href: r.url, target: "_blank", rel: "noopener noreferrer" }, r.url)) },
    { key: "kind", label: "Kind", render: (r) => (r.kind ? h("span", { class: "badge" }, r.kind) : "–") },
    { key: "words", label: "Words", num: true, render: (r) => fmt.n(r.words) },
    { key: "ms", label: "Took", num: true, render: (r) => fmt.ms(r.ms) },
  ], { rows: p.pages || [], compact: true, sortKey: null });
  const columns = columnTable(core, cols);
  const fields = fieldTable(core, p.fields || []);
  const template = (p.template || []).length
    ? h("div", { class: "row", style: { gap: "6px" } }, p.template.map((t) => h("span", { class: "badge mono", title: `${t.selector}: on ${fmt.pct(t.share, 0)} of the pages` }, `${t.name} ← ${t.selector}`)))
    : h("div", { class: "small muted" }, "No fields from the site's template: its pages share no element that holds a value of its own.");
  return card({
    title: `The table ${p.table}`,
    sub: `from ${fmt.plural(p.pages?.length || 0, "page")} read, nothing written`,
    tools: h("button", { class: "btn sm primary", onclick: start }, icon("play"), "Crawl it"),
    body: h("div", {},
      h("div", { class: "grid g2" },
        h("div", {}, h("div", { class: "cr-sub" }, `${cols.length} columns`), columns, h("div", { class: "cr-sub" }, "Made with"), sqlBlock(p.sql)),
        h("div", {}, h("div", { class: "cr-sub" }, "Pages read"), pages,
          h("div", { class: "cr-sub" }, "Learned from the site's template"), template,
          (p.problems || []).length ? [h("div", { class: "cr-sub" }, "Not read"), h("div", { class: "small" }, p.problems.map((x) => h("div", { class: "ellipsis" }, `${x.url}: ${x.message}`)))] : null)),
      h("div", { class: "cr-sub" }, "Fields found on the pages"),
      h("div", { class: "small muted", style: { marginBottom: "6px" } }, "Each page's fields go in its data column; the common ones are columns as well. A field becomes a column during the crawl once enough pages have it."),
      fields,
      h("details", { class: "more" }, h("summary", {}, "Rows as they would be written"), h("pre", { class: "code", style: { maxHeight: "360px", overflow: "auto" } }, JSON.stringify(p.rows || [], null, 2)))),
  });
}

function sourceText(c) {
  const s = c.source || {};
  if (s.kind === "base") return "the page";
  if (s.kind === "rule") return "selector " + s.path;
  if (s.kind === "path") return s.path;
  return "the table";
}

function columnTable(core, cols) {
  const { h, table, fmt } = core;
  return table([
    { key: "name", label: "Column", render: (c) => h("b", { class: "mono" }, c.name) },
    { key: "ty", label: "Type", render: (c) => h("span", { class: "badge mono" }, String(c.ty).toUpperCase()) },
    { key: "source", label: "From", sort: sourceText, render: (c) => h("span", { class: "small mono dim" }, sourceText(c)) },
    { key: "fill", label: "Filled", num: true, render: (c) => (c.source?.kind === "base" ? "–" : fmt.pct(c.fill, 0)) },
    { key: "added_at", label: "Since", num: true, title: "When the column was added: with the table, or at a page of the crawl", render: (c) => (c.added_at ? h("span", { class: "badge accent", title: `added at page ${c.added_at}` }, `page ${fmt.n(c.added_at)}`) : h("span", { class: "muted small" }, "the start")) },
  ], { rows: cols, compact: true, maxHeight: "420px" });
}

function fieldTable(core, fields) {
  const { h, table, shareBar } = core;
  const ex = (v) => { const t = typeof v === "string" ? v : JSON.stringify(v); return t == null ? "–" : t.length > 80 ? t.slice(0, 80) + "…" : t; };
  return table([
    { key: "path", label: "Field", render: (r) => h("span", { class: "mono small" }, r.path) },
    { key: "share", label: "Of the pages", num: true, render: (r) => shareBar(r.share) },
    { key: "type", label: "Reads as", render: (r) => h("span", { class: "badge mono" }, String(r.type || "").toUpperCase()) },
    { key: "example", label: "Example", sortable: false, render: (r) => h("span", { class: "small dim ellipsis", style: { display: "block", maxWidth: "340px" }, title: ex(r.example) }, ex(r.example)) },
    { key: "column", label: "Column", render: (r) => (r.column ? h("b", { class: "mono small" }, r.column) : h("span", { class: "muted small" }, "data only")) },
  ], { rows: fields, compact: true, maxHeight: "420px", sortKey: "share" });
}

// ---------------------------------------------------------------- one crawl

function crawlPage(root, ctx, id) {
  const core = { ...ctx.core, charts: ctx.charts };
  const { h, icon, card, table, fmt, empty, failure, toast, every, sqlBlock } = core;
  let gone = false, st = null;

  const title = h("h1", {}, id), phase = h("span"), sub = h("p");
  const pauseBtn = h("button", { class: "btn sm", onclick: () => act(st?.phase === "paused" ? "resume" : "pause") });
  const stopBtn = h("button", { class: "btn sm danger", onclick: () => core.confirm({ title: "Stop this crawl?", body: "The requests out finish and their rows are written; the queue is dropped.", ok: "Stop", danger: true }).then((ok) => ok && act("stop")) }, icon("stop"), "Stop");
  const sqlLink = h("a", { class: "btn sm", title: "Open the table in the SQL workbench" }, icon("sql"), "Query");
  const forgetBtn = h("button", { class: "btn sm ghost", title: "Take it off the list (the table stays)", onclick: () => forget() }, icon("trash"), "Forget");
  root.append(h("div", { class: "page-head" },
    h("div", {}, h("a", { class: "small", href: ctx.href() }, "← Crawls"), h("div", { class: "row", style: { gap: "10px" } }, title, phase), sub),
    h("div", { class: "tools" }, pauseBtn, stopBtn, sqlLink, forgetBtn)));
  const errorBox = h("div"), whyBox = h("div");
  const bar = h("i", { style: { width: "0%" } });
  root.append(errorBox, whyBox, h("div", { class: "cr-bar", title: "Pages handed out of the most it fetches" }, bar));

  const k = { pages: kpi(core, "Pages fetched"), rows: kpi(core, "Rows written"), queue: kpi(core, "Queued"), cols: kpi(core, "Columns"), shards: kpi(core, "Shards"), misses: kpi(core, "Not pages") };
  root.append(h("div", { class: "grid g6" }, Object.values(k).map((x) => x.el)));

  // xyChart reads its marks from these options each time it draws.
  const chartOpts = { series: [], marks: [], height: 210, xfmt: (x) => fmt.time(x), yfmt: (v) => fmt.si(v), label: "pages and rows a second", emptyText: "a second or two…" };
  const chart = core.charts.xyChart(chartOpts);
  const shardsBox = h("div"), growText = h("span", { class: "sub" });
  root.append(h("div", { class: "grid g2", style: { marginTop: "14px" } },
    card({ title: "Pages and rows a second", body: chart }),
    h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Shards"), growText), h("div", { class: "bd" }, shardsBox))));

  const colsBox = h("div"), fieldsBox = h("div"), sqlBox = h("div"), templateBox = h("div");
  const schemaSub = h("span", { class: "sub" });
  root.append(h("section", { class: "card", style: { marginTop: "14px" } },
    h("div", { class: "hd" }, h("h3", {}, "The table's schema"), schemaSub),
    h("div", { class: "bd" }, h("div", { class: "grid g2" },
      h("div", {}, h("div", { class: "cr-sub" }, "Columns"), colsBox, h("div", { class: "cr-sub" }, "Made with"), sqlBox, templateBox),
      h("div", {}, h("div", { class: "cr-sub" }, "Fields on the pages so far"), fieldsBox)))));

  // Search the rows.
  const q = h("input", { class: "input", type: "search", placeholder: "Words in the pages, or empty for the latest rows", "aria-label": "Search the table" });
  const hits = h("div");
  const searchSql = h("div");
  root.append(h("div", { class: "grid g2", style: { marginTop: "14px" } },
    card({ title: "Search the table", sub: "through ChloeDB", body: h("div", {}, h("form", { class: "cr-search", onsubmit: (e) => { e.preventDefault(); search(); } }, q, h("button", { class: "btn", type: "submit" }, icon("search"), "Search")), hits, searchSql) }),
    card({ title: "What it did", body: h("div", { style: { maxHeight: "520px", overflow: "auto" } }, h("div", { "data-events": "" })) })));
  const eventsBox = root.querySelector("[data-events]");

  const recent = table([
    { key: "at_ms", label: "At", num: true, render: (r) => h("span", { class: "small muted", title: fmt.datetime(r.at_ms) }, fmt.time(r.at_ms)) },
    { key: "url", label: "Page", render: (r) => h("div", {}, h("div", { class: "ellipsis cr-url" }, r.title || "(no title)"), h("a", { class: "tiny ellipsis cr-url", href: r.url, target: "_blank", rel: "noopener noreferrer" }, r.url)) },
    { key: "kind", label: "Kind", render: (r) => (r.kind ? h("span", { class: "badge" }, r.kind) : "–") },
    { key: "fields", label: "Fields", num: true },
    { key: "words", label: "Words", num: true, render: (r) => fmt.n(r.words) },
    { key: "bytes", label: "Size", num: true, render: (r) => fmt.bytes(r.bytes) },
    { key: "ms", label: "Took", num: true, render: (r) => fmt.ms(r.ms) },
  ], { rows: [], compact: true, maxHeight: "440px", empty: empty("info", "No pages yet") });
  const problems = table([
    { key: "at_ms", label: "At", num: true, render: (r) => h("span", { class: "small muted", title: fmt.datetime(r.at_ms) }, fmt.time(r.at_ms)) },
    { key: "kind", label: "Why", render: (r) => h("span", { class: "badge" }, r.kind) },
    { key: "url", label: "Page", render: (r) => h("div", {}, h("div", { class: "small" }, r.message), h("a", { class: "tiny ellipsis cr-url", href: /^https?:/.test(r.url) ? r.url : null, target: "_blank", rel: "noopener noreferrer" }, r.url)) },
  ], { rows: [], compact: true, maxHeight: "440px", empty: empty("okc", "No problems") });
  const problemsSub = h("span", { class: "sub" });
  root.append(h("div", { class: "grid g2", style: { marginTop: "14px" } },
    card({ title: "Pages read last", body: recent, flush: true }),
    h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Problems"), problemsSub), h("div", { class: "bd flush" }, problems))));

  async function act(a) {
    try { await ctx.api(`/v1/crawls/${encodeURIComponent(id)}/${a}`, { method: "POST", body: {} }); await tick(); }
    catch (e) { toast(e.message, "bad"); }
  }
  async function forget() {
    try { await ctx.api(`/v1/crawls/${encodeURIComponent(id)}`, { method: "DELETE" }); location.hash = ctx.href(); }
    catch (e) { toast(e.message, "bad"); }
  }

  // Beside the page, up to three of the site's own fields: a selector's,
  // then short values (numbers, keywords) read from the pages.
  const shown = () => {
    const rank = (c) => (c.source?.kind === "rule" ? 0 : c.source?.kind === "path" && !c.source.path.startsWith("meta.") ? (c.ty === "text" ? 2 : 1) : 9);
    return (st?.plan?.columns || []).filter((c) => rank(c) < 9).sort((a, b) => rank(a) - rank(b) || b.fill - a.fill).slice(0, 3).map((c) => c.name);
  };
  const short = (v) => { const t = v == null ? "–" : Array.isArray(v) ? v.join(", ") : String(v); return t.length > 60 ? t.slice(0, 60) + "…" : t; };
  async function search() {
    if (!st?.plan) { hits.replaceChildren(h("div", { class: "small muted" }, "The table is made once the sample is read.")); return; }
    const t = quote(st.table), extra = shown();
    const cols = ["url", "title", ...extra].map(quote).join(", ");
    const text = q.value.trim();
    const sql = text
      ? `SELECT ${cols}, score() FROM ${t}\nWHERE MATCH(body, :q)\nORDER BY bm25(body, :q, syntax => 'plain') DESC LIMIT 10`
      : `SELECT ${cols} FROM ${t} ORDER BY "fetched" DESC LIMIT 10`;
    hits.replaceChildren(h("div", { class: "small muted" }, "Asking…"));
    try {
      const j = await core.api("/v1/query", { body: { sql, params: text ? { q: text } : {}, objects: true }, timeout: 15000 });
      if (gone) return;
      const rows = j.rows || [];
      hits.replaceChildren(rows.length ? table([
        { key: "title", label: "Page", render: (r) => h("div", { style: { minWidth: "220px" } }, h("div", { class: "ellipsis cr-url" }, r.title || "(no title)"), h("a", { class: "tiny ellipsis cr-url", href: r.url, target: "_blank", rel: "noopener noreferrer" }, r.url)) },
        ...extra.map((c) => ({ key: c, label: c, render: (r) => h("span", { class: "small", title: r[c] == null ? "" : String(r[c]) }, short(r[c])) })),
        ...(text ? [{ key: "score()", label: "Score", num: true, render: (r) => fmt.si(r["score()"] ?? r.score) }] : []),
      ], { rows, compact: true, sortKey: null }) : empty("search", "No rows match"), h("div", { class: "tiny muted", style: { marginTop: "6px" } }, `${fmt.plural(rows.length, "row")} in ${fmt.ms(j.took_ms)}`));
      searchSql.replaceChildren(h("details", { class: "more" }, h("summary", {}, "The query"), sqlBlock(sql), h("a", { class: "btn sm ghost", style: { marginTop: "6px" }, href: "#/sql?q=" + encodeURIComponent(sql.replace(/:q/g, `'${text.replace(/'/g, "''")}'`)) }, icon("play"), "Open in the workbench")));
    } catch (e) { if (!gone) hits.replaceChildren(h("div", { class: "error" }, e.message)); }
  }

  let lastEvents = -1, lastCols = -1, lastFields = "", searched = false;
  async function tick() {
    let s;
    try { s = await ctx.api(`/v1/crawls/${encodeURIComponent(id)}`, { timeout: 8000 }); }
    catch (e) {
      if (gone) return;
      if (e.status === 404) { root.replaceChildren(empty("route", `No crawl ${id}`, "The crawler forgets its crawls when it restarts; the tables stay.", h("a", { class: "btn sm", href: ctx.href() }, "All crawls"))); gone = true; return; }
      errorBox.replaceChildren(failure(e, "This crawl"));
      return;
    }
    if (gone) return;
    st = s;
    const c = s.counts, fr = s.frontier, g = s.grow || {};
    const active = ACTIVE.has(s.phase);
    title.textContent = s.table;
    phase.replaceChildren(phasePill(core, s.phase));
    sub.replaceChildren(s.spec.seeds.join(" "), h("span", { class: "muted" }, ` · started ${fmt.ago(s.started_ms)}`, s.finished_ms ? `, took ${fmt.dur((s.finished_ms - s.started_ms) / 1000)}` : ""));
    pauseBtn.replaceChildren(icon(s.phase === "paused" ? "play" : "pause"), s.phase === "paused" ? "Resume" : "Pause");
    const showIf = (b, yes) => { b.style.display = yes ? "" : "none"; };
    showIf(pauseBtn, s.phase === "crawling" || s.phase === "paused" || s.phase === "sampling");
    showIf(stopBtn, active && s.phase !== "stopping");
    showIf(forgetBtn, !active);
    sqlLink.href = "#/sql?q=" + encodeURIComponent(`SELECT * FROM ${quote(s.table)} LIMIT 20;`);
    errorBox.replaceChildren(s.error ? h("div", { class: "error", style: { marginBottom: "12px" } }, s.error) : "");
    // Why a crawl ended, so that a crawl cut short by its depth or its
    // page limit says so (and what to raise), not only "done".
    const why = s.why_done || (s.phase === "done" && s.beyond_depth ? `${fmt.n(s.beyond_depth)} links on the deepest pages were not followed (at most ${s.spec.max_depth} links deep)` : null);
    whyBox.replaceChildren(why ? h("div", { class: "notice", style: { marginBottom: "12px" }, "data-why": "" }, h("b", {}, "Why it ended: "), why) : "");
    bar.style.width = Math.min(100, (fr.handed / Math.max(1, s.spec.max_pages)) * 100) + "%";

    const series = s.series || [];
    const pr = rates(series, "fetched"), wr = rates(series, "written");
    k.pages.set(fmt.n(c.fetched), active ? `${fmt.si(s.rate.fetched)}/s` : `of ${fmt.si(s.spec.max_pages)} at most`, pr.slice(-60).map((p) => p[1]));
    const rowsNote = [active ? `${fmt.si(s.rate.written)}/s` : null, c.refused ? `${fmt.n(c.refused)} refused` : null, !active && c.duplicates ? `${fmt.n(c.duplicates)} duplicates` : null].filter(Boolean).join(", ");
    k.rows.set(fmt.n(c.written), rowsNote, wr.slice(-60).map((p) => p[1]));
    k.queue.set(fmt.n(fr.queued), `${fr.in_flight} out now, ${fmt.si(fr.seen)} links seen` + (s.beyond_depth ? `, ${fmt.si(s.beyond_depth)} past depth ${s.spec.max_depth}` : ""));
    const cols = s.plan?.columns || [];
    k.cols.set(s.plan ? String(cols.length) : "–", s.plan ? (c.added_columns ? `${c.added_columns} added while crawling` : "all from the sample") : `made after ${s.spec.sample} pages`);
    k.shards.set(g.shards?.length ? String(g.shards.length) : "–", g.policy?.split_rows ? `one more past ${fmt.si(g.policy.split_rows)} rows` : "growth is off");
    const misses = Object.entries(c.misses || {});
    const nMiss = misses.reduce((a, [, v]) => a + v, 0);
    k.misses.set(fmt.n(nMiss), misses.length ? misses.map(([w, v]) => `${v} ${w}`).join(", ") : "every answer was a page");

    const marks = [...(g.events || []).filter((e) => e.kind === "shard"), ...(s.events || []).filter((e) => e.kind === "schema")]
      .filter((e) => series.length && e.at_ms >= series[0].t_ms && e.at_ms <= series[series.length - 1].t_ms).map((e) => ({ x: e.at_ms, label: e.kind === "shard" ? "+ shard" : "table made" }));
    chartOpts.marks = marks;
    chart.update([{ name: "pages fetched", points: pr, area: true }, { name: "rows written", points: wr }]);

    growText.textContent = growPolicy(core, g) || "";
    shardsBox.replaceChildren(shardBars(core, g));

    schemaSub.textContent = s.plan ? `${s.plan.data ? "every field in data JSON" : ""}${s.plan.embed ? ` · emb from title and body (${s.plan.embed})` : ""} · read from ${fmt.plural(s.pages_seen, "page")}` : `made after ${s.spec.sample} pages`;
    if (cols.length !== lastCols) {
      lastCols = cols.length;
      colsBox.replaceChildren(s.plan ? columnTable(core, cols) : empty("clock", "Reading a sample", `The table is made from the first ${s.spec.sample} pages.`));
      sqlBox.replaceChildren(s.sql ? sqlBlock(s.sql) : h("div", { class: "small muted" }, "–"));
      templateBox.replaceChildren(...((s.template || []).length ? [h("div", { class: "cr-sub" }, "Learned from the site's template"), h("div", { class: "row", style: { gap: "6px" } }, s.template.map((t) => h("span", { class: "badge mono", title: `on ${fmt.pct(t.share, 0)} of the sample` }, `${t.name} ← ${t.selector}`)))] : []));
    }
    const fkey = (s.fields || []).map((x) => x.path + (x.column || "") + Math.round(x.share * 100)).join();
    if (fkey !== lastFields) { lastFields = fkey; fieldsBox.replaceChildren(fieldTable(core, s.fields || [])); }

    if ((s.events || []).length !== lastEvents) { lastEvents = s.events.length; eventsBox.replaceChildren(eventList(core, s.events)); }
    recent.update(s.recent || []);
    problems.update(s.problems || []);
    problemsSub.textContent = s.problems?.length ? `the last ${s.problems.length}` : "";
    if (!searched && s.plan && c.written > 0) { searched = true; search(); }
  }
  // Every second while it runs, every 10 s after.
  let asked = 0;
  const stopTick = every(1000, async () => {
    if (gone || (st && !ACTIVE.has(st.phase) && Date.now() - asked < 10000)) return;
    asked = Date.now();
    await tick();
  });
  return () => { gone = true; stopTick(); };
}
