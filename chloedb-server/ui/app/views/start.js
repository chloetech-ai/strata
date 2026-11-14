// Start here: what a person who just started a server does first, in
// order, each step ticked once it is done here: put data in (a file, or
// sample rows), search it, search it by meaning, save a search and call
// it from code, give an agent the tools; then what to set before the
// server holds anything that matters. Home sends a server without tables
// of its own here.

import { h, icon, clear, api, card, fmt, toast, gate, can, copy, store, bus } from "../core.js";
import { state, isSandbox, isProduction, environment, clusterName, isCoordinator, kubeInfo, managed, specName } from "../data.js";
import { tables, invalidate, isTestModel } from "../catalog.js";
import { isMine, firstMine } from "../owners.js";
import { useFromCode } from "./saved.js";
import { quoteName } from "../sqledit.js";

export default { id: "start", title: "Start here", icon: "rocket", group: "Build", range: false, keywords: "start getting started first steps onboarding sample data tutorial quickstart sandbox connect code agent mcp", render };

/** Sample rows: a library's help desk, which reads well by words and by meaning. */
export const SAMPLE = {
  table: "library_help",
  create: "CREATE TABLE library_help (id INT PRIMARY KEY, title TEXT, body TEXT, topic KEYWORD, updated TIMESTAMP)",
  rows: [
    ["How do I renew a book?", "You can renew a loan twice, online or at the desk, as long as nobody has reserved the book. Each renewal adds three weeks.", "loans"],
    ["What happens if I return something late?", "There is no fine for late books. After four weeks overdue the item is billed at its replacement cost, refunded when it comes back.", "loans"],
    ["How many items can I borrow at once?", "An adult card holds up to 30 items, a child's card 20. E-books and audiobooks count separately, five of each.", "loans"],
    ["Reserving a book that is out", "Place a hold from the catalogue. We email you when it is ready, and keep it on the hold shelf for seven days.", "holds"],
    ["Getting a library card", "Anyone who lives, works or studies in the city can have a card for free. Bring an ID and something with your address on it.", "cards"],
    ["I lost my library card", "Report it at once so nobody else can use it. A replacement costs two dollars; your loans and holds stay on the new card.", "cards"],
    ["Reading e-books on a phone or e-reader", "Install the Libby app, add your card, and borrow e-books for 21 days. They return themselves, so they are never late.", "digital"],
    ["Wi-Fi and computers", "Wi-Fi is free with no password. Public computers can be booked for two hours a day with your card number.", "spaces"],
    ["Printing and scanning", "Print from any library computer or send a file from your laptop. Black and white pages cost ten cents; scanning to email is free.", "spaces"],
    ["Booking a study room", "Rooms for two to eight people can be booked up to two weeks ahead, for three hours at a time.", "spaces"],
    ["Opening hours on holidays", "All branches close on public holidays. The day before, they close at five. The website shows each branch's hours.", "visit"],
    ["Bringing children to story time", "Story time for toddlers is on Tuesday and Thursday mornings; no booking needed. Babies are welcome too.", "events"],
    ["Donating books", "We take recent, clean books in good shape at the central branch. Textbooks and encyclopedias we cannot use.", "visit"],
    ["Can I suggest a book for the library to buy?", "Yes: fill in the purchase suggestion form. If we buy it, you get the first hold on it.", "holds"],
    ["Returning books when the library is closed", "Every branch has a drop box outside, open day and night. Items are checked in the next morning.", "loans"],
    ["Accessibility", "All branches have step-free entrances. Large-print books, audiobooks and a reading magnifier are at the central branch.", "visit"],
    ["Interlibrary loans", "If we do not have a book, we can borrow it from another library for you. It usually arrives in two weeks.", "holds"],
    ["Keeping my reading history private", "Your history is not kept unless you switch it on in your account. Staff never share what you borrow.", "cards"],
  ],
};

/** Creates the sample table and writes its rows: resolves the table's name. */
export async function loadSample() {
  await api("/v1/sql", { body: { sql: SAMPLE.create } });
  const day = 86400e3, now = Date.now();
  const q = (s) => `'${String(s).replace(/'/g, "''")}'`;
  const values = SAMPLE.rows.map(([title, body, topic], i) => `(${i + 1}, ${q(title)}, ${q(body)}, ${q(topic)}, ${q(new Date(now - (i * 3 + 1) * day).toISOString())})`);
  await api("/v1/sql", { body: { sql: `INSERT INTO ${SAMPLE.table} (id, title, body, topic, updated) VALUES ${values.join(", ")}` } });
  invalidate();
  await tables(true);
  return SAMPLE.table;
}

function render(page) {
  const body = h("div", { class: "stack" });
  page.append(body);
  let alive = true;

  async function draw() {
    let ts = [], fns = [];
    try { ts = await tables(true); } catch { /* shown as not done */ }
    try { fns = ((await api("/v1/functions")).functions || []).filter((f) => f.current != null && f.versions.some((v) => v.version === f.current && v.kind === "table")); } catch { /* none */ }
    if (!alive) return;
    const mine = ts.filter((t) => isMine(t.table));
    const first = firstMine(mine);
    // A test model (hash:N) hashes words: vectors, but no meaning.
    const embeds = (t, real) => (t.short?.fields || []).some((f) => f.type === "vector" && f.embed && isTestModel(f.embed.model) !== real);
    const meaning = mine.find((t) => embeds(t, true));
    const testOnly = !meaning && mine.find((t) => embeds(t, false));
    const steps = [
      { done: mine.length > 0, title: "Put your data in", what: mine.length ? `${fmt.plural(mine.length, "table")} of yours: ${mine.slice(0, 4).map((t) => t.table).join(", ")}${mine.length > 4 ? "…" : ""}.` : "A CSV, JSON, Parquet or SQLite file becomes a table: its fields are found from the file, and text is searchable at once.",
        act: [gate(h("a", { class: "btn primary", href: "#/import" }, icon("import"), "Import a file"), "write"),
          mine.some((t) => t.table === SAMPLE.table) ? null : gate(h("button", { class: "btn", onclick: async (e) => { e.target.disabled = true; try { const n = await loadSample(); toast(`${n}: ${SAMPLE.rows.length} rows`, "ok"); location.hash = `#/table/${n}?tab=search`; } catch (err) { toast(err.message, "bad"); e.target.disabled = false; } } }, icon("table"), `Or try sample rows (${SAMPLE.rows.length} help articles)`), "admin"),
          gate(h("a", { class: "btn ghost", href: "#/connectors", title: "Keep data coming from Prometheus, Jira, ServiceNow, Oxidized, a mailbox or a webhook" }, icon("streams"), "Or connect a system"), "admin")] },
      { done: !!first && !!store.get(`table.search.${first.table}`, null)?.q, title: "Search it", what: "Type a question: rows with some of its words come back, the ones with all of them first. No query language needed.",
        act: [first ? h("a", { class: "btn", href: `#/table/${encodeURIComponent(first.table)}?tab=search` }, icon("search"), `Search ${first.table}`) : null] },
      { done: !!meaning, title: "Search by meaning", what: meaning ? `${meaning.table} has vectors: its search finds rows that say the same thing in other words.` : testOnly ? `${testOnly.table} has vectors from a test model, which hashes words and knows no meaning: pick a real model for it in Models.` : "One click gives a table vectors from its text, so that \"can I keep a book longer\" finds \"How do I renew a book?\".",
        act: [testOnly ? h("a", { class: "btn", href: "#/models" }, icon("sparkle"), "Change the model") : first ? h("a", { class: "btn", href: `#/table/${encodeURIComponent((meaning || first).table)}?tab=search` }, icon("sparkle"), meaning ? "Try it" : "Add meaning") : null] },
      { done: fns.length > 0, title: "Save a search, call it from code", what: fns.length ? `${fmt.plural(fns.length, "saved search", "saved searches")}: ${fns.slice(0, 3).map((f) => f.name).join(", ")}.` : "A search you save is a stored function: one definition that SQL, an HTTP call, Python, JavaScript and agents all call.",
        act: [h("a", { class: "btn", href: "#/saved" }, icon("bookmark"), "Saved searches")] },
      { done: false, optional: true, title: "Give an agent the tools", what: "The server speaks MCP: an agent lists the tables, searches, and calls your saved searches as tools (fn_<name>), with the key you give it.",
        act: [h("button", { class: "btn", onclick: () => copy(`claude mcp add --transport http chloedb ${location.origin}/v1/mcp --header "Authorization: Bearer $CHLOEDB_TOKEN"`, "Command copied: run it where Claude Code runs") }, icon("copy"), "Copy the Claude Code command"), h("a", { class: "btn ghost", href: "#/access?tab=keys&new=1" }, icon("key"), "Make a read key for it")] },
    ];
    const n = steps.filter((s) => s.done).length;
    // Its steps done, a bare address opens Home again (main.js).
    store.set("start.done", n >= steps.filter((s) => !s.optional).length);
    clear(body,
      h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Start here"), h("p", {}, `${clusterName() || location.host}: your data, searchable by words and by meaning, from any language or agent. ${n} of ${steps.filter((s) => !s.optional).length} steps done.`))),
      modeCard(),
      h("ol", { class: "start-steps" }, steps.map((s, i) => h("li", { class: s.done ? "done" : "" },
        h("span", { class: "start-n" }, s.done ? icon("check") : String(i + 1)),
        h("div", { class: "grow stack", style: { gap: "4px" } }, h("b", {}, s.title, s.optional ? h("span", { class: "small muted" }, " · when you are ready") : null), h("div", { class: "small dim" }, s.what), h("div", { class: "row", style: { gap: "8px", marginTop: "4px", flexWrap: "wrap" } }, s.act))))),
      first ? card({ title: "From your code", sub: `the first rows of ${first.table}, from anywhere`, body: useFromCode({ sql: `SELECT * FROM ${quoteName(first.table)} LIMIT 5`, params: {} }) }) : null,
      card({ title: "Before it holds anything that matters", sub: "what a sandbox leaves open", body: h("ul", { class: "start-prod" },
        h("li", {}, h("b", {}, "Name the environment"), ": ", setHow("--environment", "spec.environment", "production"), ", so that this page asks before writes and drops."),
        h("li", {}, h("b", {}, "Close it with keys"), ": ", managed() ? [h("code", {}, "spec.tokenSecret"), " for the cluster's own token"] : [h("code", {}, "--token"), " for the server's own"], ", then ", h("a", { href: "#/access?tab=keys" }, "API keys"), " per person, app and agent, limited to their tables."),
        h("li", {}, h("b", {}, "Keep the audit log on disk"), " and ", h("b", {}, "send alerts somewhere"), ": ", h("a", { href: "#/home" }, "Home"), "'s readiness card lists what is set and what is not.")) }));
  }

  /** How to set one thing, where this cluster takes it: the StrataCluster's spec under the operator, else a flag. */
  function setHow(flag, field, value) {
    return managed() ? ["set ", h("code", {}, `${field}: ${value}`), ` (or staging) in the StrataCluster ${specName()}`] : ["start the server with ", h("code", {}, `${flag} ${value}`), " (or staging)"];
  }

  function modeCard() {
    const env = environment();
    if (isSandbox()) return h("div", { class: "mode-card sandbox" }, icon("check"), h("div", {}, h("b", {}, "Sandbox"), h("div", { class: "small" }, "No environment is set and this page is opened on this machine: writes, imports and drops run without a second ask. Nothing here is treated as production.")));
    // LIV-10: the same advice as below, for a cluster run by hand or by the operator.
    if (env == null && isProduction()) return h("div", { class: "mode-card prod" }, icon("lock"), h("div", {}, h("b", {}, "Treated as production"), h("div", { class: "small" }, "No environment is set and this page is not on localhost, so writes in the workbench wait for a check. Name the environment to say what it is (", managed() ? [h("code", {}, "spec.environment"), ` in the StrataCluster ${specName()}`] : h("code", {}, "--environment NAME"), "): production keeps the checks; staging or dev drops them, for a cluster that holds nothing that matters.")));
    return h("div", { class: "mode-card" + (isProduction() ? " prod" : "") }, icon(isProduction() ? "lock" : "info"), h("div", {}, h("b", {}, `Environment: ${env}`), h("div", { class: "small" }, isProduction() ? "Production: writes and drops ask first and are kept in the audit log." : "Not production: writes run without a second ask.")));
  }

  kubeInfo();
  draw();
  // Its advice names the spec once the server says an operator runs it.
  const off = bus.on("kube", () => { if (alive) draw(); });
  return () => { alive = false; off(); };
}
