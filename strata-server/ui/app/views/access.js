// Access: who you are, how the server is protected, the API keys (made
// here, the secret shown once), what each scope may do, and the audit log
// of every admin request and every refusal.

import { h, icon, clear, fmt, card, api, every, toast, confirm, modal, seg, pill, empty, table, copy, failure, debounce, bus, setToken } from "../core.js";
import { state, refreshWho } from "../data.js";

export default { id: "access", title: "Access", icon: "access", group: "Admin", keywords: "keys api key token scopes audit log security users permissions", render };

const SCOPES = [
  ["read", "Read", "queries, search, schema, stats, the series and alerts"],
  ["write", "Write", "also writes, imports, streams, the agent, INSERT, UPDATE and DELETE"],
  ["admin", "Admin", "also DDL, keys, the audit log, alert rules, checkpoints and vacuum, export, other clients' queries and the logs"],
];
const MATRIX = [
  ["Run queries, search, explain", 1, 1, 1],
  ["Read the schema, stats, series, alerts, cluster", 1, 1, 1],
  ["Write rows, import files, stream", 0, 1, 1],
  ["INSERT, UPDATE, DELETE", 0, 1, 1],
  ["Ask the agent", 0, 1, 1],
  ["Log queries to an existing rank log", 0, 1, 1],
  ["CREATE, DROP (DDL), over HTTP and gRPC", 0, 0, 1],
  ["Import a SQLite file that creates tables", 0, 0, 1],
  ["See and cancel others' queries; read the logs", 0, 0, 1],
  ["Checkpoint, vacuum, verify, promote", 0, 0, 1],
  ["Export the database", 0, 0, 1],
  ["Manage keys, alert rules; read the audit log", 0, 0, 1],
];

function render(page, r) {
  const you = h("div"), posture = h("div", { class: "posture" }), keysBox = h("div"), auditBox = h("div");
  const newBtn = h("button", { class: "btn primary", onclick: () => create() }, icon("plus"), "Create API key");
  page.append(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Access"), h("p", {}, "API keys with a scope each, and a log of what was done with them.")), h("div", { class: "tools" }, newBtn)),
    h("div", { class: "grid g3", style: { marginBottom: "14px" } },
      card({ title: "You", body: you }),
      card({ title: "How this server is protected", body: posture }),
      card({ title: "What each scope may do", flush: true, body: matrix() })),
    card({ title: "API keys", sub: "kept hashed: a secret is shown once, when it is made", body: keysBox, flush: true, attrs: { style: { marginBottom: "14px" } } }),
    card({ title: "Audit log", sub: "admin requests, refusals, and what changed", body: auditBox, flush: true }));

  function drawYou() {
    const w = state.whoami;
    if (!w) { clear(you, empty("lock", "Not signed in", "This server needs a key.", h("button", { class: "btn sm primary", onclick: () => bus.emit("unauthorized") }, "Enter a key"))); return; }
    clear(you, h("div", { class: "row nw", style: { gap: "12px", marginBottom: "10px" } },
      h("div", { style: { width: "40px", height: "40px", borderRadius: "50%", background: "var(--accent-soft)", color: "var(--accent)", display: "grid", placeItems: "center", fontWeight: 700 } }, (w.name || "?").slice(0, 1).toUpperCase()),
      h("div", {}, h("div", { style: { fontWeight: 650, fontSize: "15px" } }, w.name), h("div", { class: "small muted" }, w.root ? "the server's token" : w.key ? `key ${w.key}` : "no key"))),
      h("div", { class: "row" }, pill(w.scope === "admin" ? "info" : "ok", w.scope + " scope"), w.root ? pill("info", "root") : null),
      h("div", { class: "row", style: { marginTop: "12px" } }, h("button", { class: "btn sm", onclick: () => bus.emit("unauthorized") }, icon("access"), "Use another key")));
    const a = w.auth || {};
    const items = [
      a.open ? ["bad", "Anyone who reaches it may do anything", "No token and no keys. Create an admin key to close it."] : ["ok", "Every request needs a key or the token", a.token ? "A token (--token) and keys." : "Keys only."],
      a.token ? ["ok", "The calls between servers need the token", "Replication and shard copies take only --token, never a key."] : ["warn", "No cluster token", "Servers that replicate or shard should share --token."],
      location.protocol === "https:" ? ["ok", "This page came over TLS", "Keys travel encrypted."] : location.hostname === "127.0.0.1" || location.hostname === "localhost" ? ["ok", "Served on this machine only", "Use TLS (--tls-cert, --tls-key) before serving others."] : ["warn", "Plain HTTP", "Keys cross the network in the clear: start with --tls-cert and --tls-key."],
      a.keys ? ["ok", "API keys are in use", "Give each client its own, with the least scope it needs."] : ["warn", "No API keys", "Clients share the token, which is the admin."],
    ];
    clear(posture, items.map(([lv, t, sub]) => h("div", { class: "it " + lv }, icon(lv === "ok" ? "okc" : lv === "bad" ? "bad" : "warn"), h("div", {}, h("div", { style: { fontWeight: 600 } }, t), h("div", { class: "small muted" }, sub)))));
  }

  function matrix() {
    return h("div", { class: "tbl-wrap" }, h("table", { class: "t compact" },
      h("thead", {}, h("tr", {}, h("th", {}, ""), ...["read", "write", "admin"].map((x) => h("th", { style: { textAlign: "center" } }, x)))),
      h("tbody", {}, MATRIX.map(([what, ...on]) => h("tr", {}, h("td", { class: "small" }, what), on.map((x) => h("td", { style: { textAlign: "center", color: x ? "var(--good)" : "var(--line-2)" } }, x ? icon("check") : "·")))))));
  }

  const keyTable = table([
    { key: "name", label: "name", render: (k) => h("b", {}, k.name) },
    { key: "prefix", label: "key", render: (k) => h("span", { class: "mono small" }, k.prefix + "…") },
    { key: "scope", label: "scope", render: (k) => pill(k.scope === "admin" ? "info" : k.scope === "write" ? "warn" : "ok", k.scope) },
    { key: "created_ms", label: "created", num: true, render: (k) => h("span", { title: fmt.datetime(k.created_ms) }, fmt.ago(k.created_ms)) },
    { key: "last_used_ms", label: "last used", num: true, render: (k) => h("span", { title: fmt.datetime(k.last_used_ms) }, fmt.ago(k.last_used_ms)) },
    { key: "expires_ms", label: "expires", num: true, render: (k) => k.expires_ms ? h("span", { style: { color: k.expires_ms < Date.now() ? "var(--crit-ink)" : null } }, k.expires_ms < Date.now() ? "expired" : fmt.ago(k.expires_ms)) : "never" },
    { key: "x", label: "", sortable: false, render: (k) => h("button", { class: "btn sm danger", onclick: () => revoke(k) }, icon("trash"), "Revoke") },
  ], { sortKey: "created_ms", desc: true, empty: empty("access", "No API keys", "Clients use the server's token. Create a key for each with only the scope it needs.", h("button", { class: "btn sm primary", onclick: () => create() }, icon("plus"), "Create the first key")) });

  async function loadKeys() {
    try {
      const j = await api("/v1/admin/keys");
      clear(keysBox, keyTable, j.file ? h("div", { class: "small muted", style: { padding: "8px 14px" } }, `Kept in ${j.file}`) : null);
      keyTable.update(j.keys);
    } catch (e) { clear(keysBox, h("div", { style: { padding: "14px" } }, failure(e, "The API keys"))); }
  }

  async function revoke(k) {
    if (!await confirm({ title: `Revoke ${k.name}?`, body: `Requests with ${k.prefix}… are refused from now on. This cannot be undone.`, ok: "Revoke", danger: true })) return;
    try { await api("/v1/admin/keys/" + k.id, { method: "DELETE" }); toast(`${k.name} revoked`, "ok"); loadKeys(); loadAudit(); }
    catch (e) { toast(e.message, "bad"); }
  }

  function create() {
    let scope = "read", days = null;
    const name = h("input", { class: "input", placeholder: "search-frontend", style: { width: "100%" } });
    const scopeCards = h("div", { class: "grid g3" });
    const drawScopes = () => clear(scopeCards, SCOPES.map(([v, t, sub]) => h("button", { type: "button", class: "card", style: { padding: "10px 12px", textAlign: "left", cursor: "pointer", borderColor: scope === v ? "var(--accent)" : null, background: scope === v ? "var(--accent-soft)" : null, boxShadow: scope === v ? "0 0 0 3px var(--focus)" : null }, onclick: () => { scope = v; drawScopes(); } }, h("b", {}, t), h("div", { class: "small muted", style: { marginTop: "3px" } }, sub))));
    drawScopes();
    const exp = seg([[null, "never"], [7, "7 days"], [30, "30 days"], [90, "90 days"], [365, "a year"]], null, (v) => (days = v));
    const go = h("button", { class: "btn primary" }, icon("plus"), "Create");
    const m = modal({ title: "Create an API key", wide: true, body: [h("label", { class: "f" }, "Name: who or what uses it", name), h("div", { class: "f" }, h("span", { class: "small muted" }, "Scope"), scopeCards), h("div", { class: "f" }, h("span", { class: "small muted" }, "Expires"), exp)], foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go] });
    go.onclick = async () => {
      if (!name.value.trim()) { name.focus(); return; }
      try {
        const j = await api("/v1/admin/keys", { body: { name: name.value.trim(), scope, expires_days: days } });
        m.close();
        const curl = `curl -H "authorization: Bearer ${j.secret}" ${location.origin}/v1/health`;
        const m2 = modal({ title: "Your new key", wide: true, body: [
          h("div", { class: "notice" }, "Copy it now: the server keeps only its hash, so this is the only time it is shown."),
          h("div", { class: "row nw" }, h("div", { class: "secret grow" }, j.secret), h("button", { class: "btn primary", onclick: () => copy(j.secret, "Key copied") }, icon("copy"), "Copy")),
          h("dl", { class: "kv" }, [["name", j.key.name], ["scope", j.key.scope], ["id", j.key.id], ["expires", j.key.expires_ms ? fmt.datetime(j.key.expires_ms) : "never"]].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
          h("div", { class: "small muted" }, "Try it:"), h("div", { class: "row nw" }, h("pre", { class: "code grow" }, curl), h("button", { class: "btn icon sm", onclick: () => copy(curl) }, icon("copy"))),
          j.closed ? h("div", { class: "notice" }, "This server was open; it now needs a key for every request. This page keeps working because it signed in with the new key.") : null,
        ], foot: [h("button", { class: "btn primary", onclick: () => m2.close() }, "Done")] });
        if (j.closed) { setToken(j.secret); refreshWho(); }
        loadKeys(); loadAudit();
      } catch (e) { toast(e.message, "bad"); }
    };
  }

  // The audit log.
  const af = { who: "", status: "", q: "" };
  const whoSel = h("select", { class: "select sm", onchange: () => { af.who = whoSel.value; drawAudit(); } });
  const statusSeg = seg([["", "all"], ["ok", "succeeded"], ["refused", "refused"]], "", (v) => { af.status = v; drawAudit(); });
  const search = h("input", { class: "input sm", placeholder: "Search route or detail…", style: { width: "220px" }, oninput: debounce(() => { af.q = search.value.toLowerCase(); drawAudit(); }, 150) });
  const auditTable = table([
    { key: "ts_ms", label: "time", num: true, render: (e) => h("span", { class: "nowrap", title: fmt.datetime(e.ts_ms) }, fmt.datetime(e.ts_ms)) },
    { key: "who", label: "who", render: (e) => h("span", {}, h("b", {}, e.who), e.key ? h("span", { class: "small muted mono" }, " " + e.key) : null) },
    { key: "scope", label: "scope", render: (e) => e.scope ? h("span", { class: "badge" }, e.scope) : "–" },
    { key: "route", label: "request", render: (e) => h("span", { class: "mono small" }, `${e.method} ${e.route}`) },
    { key: "status", label: "status", num: true, render: (e) => h("span", { class: "pill " + (e.status >= 400 ? "bad" : "ok"), style: { padding: "0 8px" } }, e.status) },
    { key: "ms", label: "ms", num: true, render: (e) => fmt.ms(e.ms) },
    { key: "detail", label: "what", render: (e) => e.detail ? h("span", { class: "small mono ellipsis", style: { maxWidth: "380px", display: "inline-block" }, title: e.detail }, e.detail) : h("span", { class: "muted" }, "–") },
    { key: "request_id", label: "request", render: (e) => e.request_id ? h("a", { class: "mono small", href: "#/logs?request=" + encodeURIComponent(e.request_id) }, e.request_id.slice(0, 10) + "…") : "–" },
  ], { sortKey: "ts_ms", desc: true, empty: empty("shield", "Nothing audited yet", "Admin requests and refused ones show here.") });
  let audit = [], auditFile = null;
  function drawAudit() {
    const whos = [...new Set(audit.map((e) => e.who))].sort();
    if (whoSel.options.length !== whos.length + 1) { clear(whoSel, h("option", { value: "" }, "everyone"), whos.map((w) => h("option", { value: w }, w))); whoSel.value = af.who; }
    auditTable.update(audit.filter((e) => (!af.who || e.who === af.who) && (!af.status || (af.status === "ok" ? e.status < 400 : e.status >= 400)) && (!af.q || `${e.route} ${e.detail || ""}`.toLowerCase().includes(af.q))));
  }
  async function loadAudit() {
    try {
      const j = await api("/v1/admin/audit?limit=1000");
      audit = j.entries; auditFile = j.file;
      if (!auditBox.firstChild || auditBox.querySelector(".error,.empty:not(td .empty)")) clear(auditBox, h("div", { class: "row", style: { padding: "10px 14px", gap: "10px" } }, whoSel, statusSeg, search, h("span", { class: "sep" }), h("span", { class: "small muted" }, auditFile ? `also written to ${auditFile}` : "in memory: --audit-log FILE keeps it")), auditTable);
      drawAudit();
    } catch (e) { clear(auditBox, h("div", { style: { padding: "14px" } }, failure(e, "The audit log"))); }
  }

  drawYou();
  loadKeys();
  if (r.params.get("new")) setTimeout(create, 50);
  const offs = [bus.on("whoami", () => { drawYou(); loadKeys(); loadAudit(); }), every(5000, loadAudit)];
  refreshWho();
  return () => offs.forEach((f) => f());
}
