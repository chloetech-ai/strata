// Connectors: other systems' data into tables (chloedb-server's
// crate::connectors). Each connector is a row of chloedb_connectors; its
// runs are rows of chloedb_connector_runs. This page shows each one's
// health, last runs and counts, runs one now, tries a config on real data
// without writing (a dry run), and adds, changes, turns off and deletes
// them. A push connector's key is shown once, when it is made or rotated.

import { h, icon, clear, fmt, api, toast, confirm, card, empty, failure, pill, copy, drawer, modal, every, can, needs } from "../core.js";

// A run under a millisecond reads "<1 ms" rather than "0".
// A target table, linked to its page in Tables (a click there does not open the drawer).
const tableLink = (t) => h("a", { href: `#/table/${encodeURIComponent(t)}?tab=data`, title: `Open ${t} in Tables`, onclick: (e) => e.stopPropagation() }, t);
const took = (v) => (v === 0 ? "<1 ms" : fmt.ms(v));

export default {
  id: "connectors", title: "Connectors", icon: "streams", group: "Data",
  keywords: "connectors data sources ingest prometheus jira servicenow oxidized email mail webhook alertmanager rest api pull push cdc health",
  render,
};

const HEALTH = {
  ok: ["ok", "ok"],
  warning: ["warn", "warning"],
  failing: ["failing", "failing"],
  invalid: ["failing", "invalid"],
  stale: ["warn", "stale"],
  running: ["info", "running"],
  pending: ["pending", "not run yet"],
  waiting: ["pending", "waiting for a push"],
  disabled: ["disabled", "off"],
};

// A starting config for each kind, for the form.
const TEMPLATES = {
  prometheus: { config: { url: "http://prometheus:9090", queries: { cpu: "avg by (device) (rate(node_cpu_seconds_total{mode!=\"idle\"}[1m]))", mem: "avg by (device) (1 - node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes)" }, key: ["device"] }, secrets: {} },
  jira: { config: { url: "https://example.atlassian.net", jql: "project = NET AND issuetype = Change", username: "bot@example.com" }, secrets: { token: "" } },
  servicenow: { config: { instance: "https://example.service-now.com", table: "change_request", username: "integration" }, secrets: { password: "" } },
  oxidized: { config: { url: "http://oxidized:8888" }, secrets: {} },
  mail: { config: {}, secrets: {} },
  webhook: { config: { preset: "alertmanager" }, secrets: {} },
  http_json: { config: { url: "https://api.example.com/items?since={cursor}", records: "$.items", cursor: "updated_at", headers: { Authorization: "Bearer {secret:token}" } }, secrets: { token: "" } },
};

// A kind that pulls with no schedule (every_s 0) runs by hand (DAP-18).
const every_text = (c, kinds) => {
  if (c.pulls) return every_pull(c);
  const k = (kinds || []).find((x) => x.kind === c.kind);
  return k?.pull ? (k.push ? "pushed, or by hand" : "by hand") : "pushed";
};
const every_pull = (c) => (c.every_s >= 3600 ? `every ${fmt.n(c.every_s / 3600, 1)} h` : c.every_s >= 60 ? `every ${fmt.n(c.every_s / 60, 1)} min` : `every ${c.every_s} s`);
const healthPill = (c) => { const [st, text] = HEALTH[c.health] || ["info", c.health]; return pill(st, text); };
const pretty = (j) => JSON.stringify(j, null, 2);

function render(page) {
  const top = h("div"), main = h("div", { class: "stack" }), side = h("div", { class: "stack" });
  page.append(top, h("div", { class: "conn-grid" }, main, side));
  let v = null;
  async function load() {
    try { v = await api("/v1/admin/connectors"); }
    catch (e) { clear(top); clear(main, failure(e, "Connectors")); clear(side); return; }
    draw();
  }
  function draw() {
    const cs = v.connectors || [];
    const counts = Object.entries(v.health || {}).map(([k, n]) => { const [st, text] = HEALTH[k] || ["info", k]; return pill(st, `${n} ${text}`); });
    clear(top, h("div", { class: "row", style: { gap: "8px", flexWrap: "wrap", marginBottom: "12px" } },
      ...counts,
      v.scheduler?.on ? h("span", { class: "small dim" }, icon("clock"), " pulls run on this server") : h("span", { class: "small warn" }, icon("warn"), " ", v.scheduler?.why || "pulls do not run here")));
    const rows = cs.map((c) => {
      const st = c.state || {};
      const last = c.runs?.[0];
      return h("tr", { class: "click", onclick: () => open(c.name) },
        h("td", {}, h("div", { class: "stack", style: { gap: "2px" } }, h("b", {}, c.name), h("span", { class: "small dim" }, kindTitle(c.kind), c.note ? ` · ${c.note}` : ""))),
        h("td", { class: "mono small" }, tableLink(c.target)),
        h("td", {}, healthPill(c), st.failures > 1 ? h("span", { class: "small dim" }, ` ×${st.failures}`) : null),
        h("td", { class: "small", style: { whiteSpace: "nowrap" } }, st.last_run_ms ? fmt.ago(st.last_run_ms) : "never", last ? h("div", { class: "dim" }, `${fmt.plural(last.rows_ || 0, "row")} · ${took(last.took_ms)}`) : null),
        h("td", { class: "num small", title: `${fmt.n(st.rows_total || 0)} rows written by ${fmt.plural(st.runs || 0, "run")}: a row upserted again counts each time, so it can be more than the rows of ${c.target}` }, fmt.si(st.rows_total || 0)),
        h("td", { class: "small dim", style: { whiteSpace: "nowrap" } }, every_text(c, v.kinds)),
        st.last_error ? h("td", { class: "small error", style: { minWidth: "200px", maxWidth: "260px", overflowWrap: "anywhere" } }, st.last_error.slice(0, 160))
          : h("td", { class: st.last_warning ? "small warn" : "small", style: { minWidth: "200px", maxWidth: "260px", overflowWrap: "anywhere" } }, st.last_warning ? st.last_warning.slice(0, 160) : ""));
    });
    clear(main, card({
      title: "Connectors", sub: `${fmt.plural(cs.length, "connector")} · configs in ${v.tables.configs}, runs in ${v.tables.runs}`,
      tools: h("button", { class: "btn sm", onclick: load }, icon("refresh"), "Refresh"),
      flush: true,
      body: cs.length ? h("div", { class: "tbl-wrap" }, h("table", { class: "t" },
        h("thead", {}, h("tr", {}, h("th", {}, "connector"), h("th", {}, "target"), h("th", {}, "health"), h("th", {}, "last run"), h("th", { class: "num", title: "Rows written by every run so far (upserts counted each time), not the rows of the table" }, "rows written"), h("th", {}, "schedule"), h("th", {}, "last error"))),
        h("tbody", {}, rows)))
        : empty("streams", "No connectors yet", "Add one: Prometheus rollups, Jira or ServiceNow tickets and changes, Oxidized configs, carriers' emails, Alertmanager or any webhook, any REST API."),
    }), sourcesCard());
    clear(side, addCard());
  }
  const kindTitle = (k) => (v?.kinds || []).find((x) => x.kind === k)?.title || k;

  function sourcesCard() {
    const ss = v.sources || [];
    return card({
      title: "Other ways in", sub: "streams and followed databases, by their last offset",
      body: h("div", { class: "stack small", style: { gap: "8px" } },
        ss.length ? h("div", { class: "tbl-wrap" }, h("table", { class: "t compact" }, h("thead", {}, h("tr", {}, h("th", {}, "source"), h("th", { class: "num" }, "offset"), h("th", {}, "last event"))),
          h("tbody", {}, ss.slice(0, 50).map((s) => h("tr", {}, h("td", { class: "mono" }, s.name), h("td", { class: "num mono" }, fmt.n(s.offset)), h("td", {}, s.updated ? fmt.ago(Date.parse(s.updated)) : "–"))))))
          : h("div", { class: "dim" }, "No stream sources yet."),
        h("div", { class: "dim" }, "OpenTelemetry logs: ", h("span", { class: "mono" }, "POST /v1/logs"), " (the Collector's otlphttp exporter). Kafka and other streams: ", h("span", { class: "mono" }, "POST /v1/stream"), " with offsets, applied once. Postgres: ", h("span", { class: "mono" }, "chloedb-server follow-postgres"), ". Files: Import and export.")),
    });
  }

  function addCard() {
    const kinds = v.kinds || [];
    const kindSel = h("select", { class: "select", "aria-label": "Kind" }, kinds.map((k) => h("option", { value: k.kind }, k.title)));
    const name = h("input", { class: "input mono", placeholder: "jira-changes", autocomplete: "off", spellcheck: "false" });
    const target = h("input", { class: "input mono", autocomplete: "off", spellcheck: "false" });
    const everyIn = h("input", { class: "input mono", type: "number", min: "10", style: { width: "110px" } });
    const conf = h("textarea", { class: "input mono", rows: 9, spellcheck: "false", style: { width: "100%", resize: "vertical" } });
    const secrets = h("textarea", { class: "input mono", rows: 3, spellcheck: "false", "data-secret": "secrets", style: { width: "100%", resize: "vertical" } });
    const about = h("div", { class: "small dim" });
    const note = h("div");
    const fill = () => {
      const k = kinds.find((x) => x.kind === kindSel.value);
      const t = TEMPLATES[k.kind] || { config: {}, secrets: {} };
      target.placeholder = k.target;
      everyIn.value = k.every_s || "";
      everyIn.disabled = !k.pull;
      conf.value = pretty(t.config);
      secrets.value = pretty(t.secrets);
      clear(about, k.about, h("ul", { style: { margin: "6px 0 0", paddingLeft: "18px" } }, k.settings.map((s) => h("li", {}, h("span", { class: "mono" }, s.name), `: ${s.about}`))));
    };
    kindSel.onchange = fill;
    if (kinds.length) fill();
    const body = () => {
      let config, sec;
      try { config = JSON.parse(conf.value || "{}"); } catch (e) { throw new Error(`config is not JSON: ${e.message}`); }
      try { sec = JSON.parse(secrets.value || "{}"); } catch (e) { throw new Error(`secrets are not JSON: ${e.message}`); }
      for (const [k, x] of Object.entries(sec)) if (x === "") delete sec[k];
      const b = { name: name.value.trim(), kind: kindSel.value, config, secrets: sec };
      if (target.value.trim()) b.target = target.value.trim();
      if (everyIn.value && !everyIn.disabled) b.every_s = Number(everyIn.value);
      return b;
    };
    const tryIt = h("button", { class: "btn" }, icon("play"), "Dry run");
    const go = h("button", { class: "btn primary" }, icon("plus"), "Add the connector");
    if (!can("admin")) { go.disabled = tryIt.disabled = true; go.title = tryIt.title = needs("admin"); }
    tryIt.onclick = async () => {
      clear(note, h("div", { class: "small dim" }, "Reading…"));
      try {
        const b = body();
        const k = kinds.find((x) => x.kind === b.kind);
        if (!k.pull) {
          const sample = await sampleFor(b.kind);
          if (sample == null) { clear(note); return; }
          b.sample = sample;
        }
        const t = await api("/v1/admin/connectors/test", { body: b, timeout: 120000 });
        clear(note, dryRun(t));
      } catch (e) { clear(note, h("div", { class: "error small" }, e.message)); }
    };
    go.onclick = async () => {
      go.disabled = true;
      try {
        const r = await api("/v1/admin/connectors", { body: body(), timeout: 60000 });
        toast(`${r.connector.name} added${r.made_target ? `, and its table ${r.connector.target}` : ""}`, "ok");
        if (r.push_secret) showKey(r.connector, r.push_secret);
        else if (r.push_key_error) toast(r.push_key_error, "info", 6000);
        name.value = "";
        clear(note);
        await load();
      } catch (e) { clear(note, h("div", { class: "error small" }, e.message)); }
      go.disabled = false;
    };
    const field = (label, el) => h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, label), el);
    return card({ title: "Add a connector", body: h("div", { class: "stack", style: { gap: "10px" } },
      field("Kind", kindSel), about,
      field("Name (lowercase, digits, - and _)", name),
      field("Target table (empty: the kind's own, made for it)", target),
      field("Pull every (seconds)", everyIn),
      field("Config (JSON; map: column → \"$.path\", \"{template}\" or a value)", conf),
      field("Secrets (JSON; kept on this server, never in a table; \"env:VAR\" reads one from its environment)", secrets),
      note, h("div", { class: "row", style: { gap: "6px" } }, tryIt, go)) });
  }

  /** What a push to try a pushed kind with: asks for it. */
  function sampleFor(kind) {
    return new Promise((resolve) => {
      const ta = h("textarea", { class: "input mono", rows: 12, spellcheck: "false", style: { width: "100%" } });
      ta.value = kind === "mail" ? "From: NOC <noc@carrier.example>\nSubject: Planned maintenance\nDate: Fri, 13 Nov 2026 03:00:00 +0000\n\nWork on circuit EXT-1001 on 20 Nov 06:00-08:00 UTC.\n"
        : pretty({ alerts: [{ status: "firing", labels: { alertname: "BGPDown", severity: "critical", instance: "r1" }, annotations: { summary: "BGP session down" }, startsAt: new Date().toISOString(), fingerprint: "abc" }] });
      let done = false;
      const ok = h("button", { class: "btn primary" }, "Try it");
      const m = modal({ title: kind === "mail" ? "A raw email to try" : "A body a system would post", body: ta, foot: [ok], wide: true, onClose: () => { if (!done) resolve(null); } });
      ok.onclick = () => { done = true; m.close(); let x = ta.value; if (kind !== "mail") { try { x = JSON.parse(x); } catch { /* sent as text */ } } resolve(x); };
    });
  }

  function dryRun(t) {
    const cols = [...new Set((t.rows || []).flatMap((r) => Object.keys(r)))];
    return h("div", { class: "stack", style: { gap: "6px" } },
      h("div", { class: "small" }, h("b", {}, fmt.plural(t.records, "record")), ` read in ${took(t.took_ms)}`, t.target_exists ? ` · rows fitted to ${t.target}` : ` · ${t.target} does not exist yet: it is made with the connector`, t.more ? " · more to read" : ""),
      t.warning ? h("div", { class: "small warn" }, icon("warn"), " ", t.warning) : null,
      t.dropped?.length ? h("div", { class: "small warn" }, icon("warn"), ` not columns of ${t.target}, left out: `, h("span", { class: "mono" }, t.dropped.join(", "))) : null,
      (t.errors || []).length ? h("div", { class: "small error" }, t.errors.slice(0, 3).join(" · ")) : null,
      cols.length ? h("div", { class: "tbl-wrap", style: { maxHeight: "260px" } }, h("table", { class: "t compact" }, h("thead", {}, h("tr", {}, cols.map((c) => h("th", {}, c)))),
        h("tbody", {}, t.rows.map((r) => h("tr", {}, cols.map((c) => h("td", { class: "mono small", style: { maxWidth: "240px", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }, title: typeof r[c] === "string" ? r[c] : JSON.stringify(r[c]) }, r[c] == null ? "" : typeof r[c] === "string" ? r[c] : JSON.stringify(r[c])))))))) : null,
      t.sample_records?.length ? h("details", {}, h("summary", { class: "small dim" }, "The records as read"), h("pre", { class: "code small", style: { maxHeight: "240px", overflow: "auto" } }, pretty(t.sample_records))) : null);
  }

  function showKey(c, secret) {
    const url = `${location.origin}/v1/connectors/${c.name}/push`;
    const curl = c.kind === "mail" ? `curl -sS -X POST ${url} \\\n  -H "Authorization: Bearer ${secret}" \\\n  -H "Content-Type: message/rfc822" --data-binary @-`
      : c.kind === "oxidized" ? `# Oxidized's config, hooks:\nhooks:\n  chloedb:\n    type: exec\n    events: [post_store]\n    cmd: 'curl -sS -X POST ${url} -H "Authorization: Bearer ${secret}" -H "Content-Type: application/json" -d "{\\"node\\":\\"$OX_NODE_NAME\\",\\"group\\":\\"$OX_NODE_GROUP\\"}"'`
      : c.config?.preset === "alertmanager" ? `# alertmanager.yml\nreceivers:\n  - name: chloedb\n    webhook_configs:\n      - url: ${url}\n        http_config:\n          authorization:\n            credentials: ${secret}`
      : `curl -sS -X POST ${url} \\\n  -H "Authorization: Bearer ${secret}" \\\n  -H "Content-Type: application/json" -d '{"id": "1", "msg": "hello"}'`;
    modal({ title: `The push key of ${c.name}`, wide: true, body: h("div", { class: "stack", style: { gap: "10px" } },
      h("div", { class: "small" }, "Shown once. It may write ", h("b", { class: "mono" }, c.target), " and nothing else; revoke or rotate it here or in Access and audit."),
      h("div", { class: "row", style: { gap: "6px" } }, h("span", { class: "mono small", style: { wordBreak: "break-all" }, "data-secret": "key" }, secret), h("button", { class: "btn sm", onclick: () => copy(secret, "Key copied") }, icon("copy"), "Copy")),
      h("pre", { class: "code small", "data-secret": "key" }, curl), h("button", { class: "btn sm", onclick: () => copy(curl, "Copied") }, icon("copy"), "Copy the snippet")) });
  }

  async function open(name) {
    let c;
    try { c = await api(`/v1/admin/connectors/${encodeURIComponent(name)}`); } catch (e) { toast(e.message, "bad"); return; }
    const out = h("div");
    const runBtn = c.pulls || (v.kinds || []).find((k) => k.kind === c.kind)?.pull ? h("button", { class: "btn sm primary" }, icon("play"), "Run now") : null;
    if (runBtn) runBtn.onclick = async () => {
      runBtn.disabled = true;
      clear(out, h("div", { class: "small dim" }, "Running…"));
      try { const r = await api(`/v1/admin/connectors/${name}/run`, { body: {}, timeout: 600000 }); clear(out, runReport(r)); load(); }
      catch (e) { clear(out, h("div", { class: "error small" }, e.message)); }
      runBtn.disabled = false;
    };
    const dry = h("button", { class: "btn sm" }, icon("eye"), "Dry run");
    dry.onclick = async () => {
      clear(out, h("div", { class: "small dim" }, "Reading…"));
      try {
        const b = { name };
        if (!c.pulls && !(v.kinds || []).find((k) => k.kind === c.kind)?.pull) { const s = await sampleFor(c.kind); if (s == null) { clear(out); return; } b.sample = s; }
        clear(out, dryRun(await api("/v1/admin/connectors/test", { body: b, timeout: 120000 })));
      } catch (e) { clear(out, h("div", { class: "error small" }, e.message)); }
    };
    const toggleBtn = h("button", { class: "btn sm" }, icon(c.enabled ? "pause" : "play"), c.enabled ? "Turn off" : "Turn on");
    toggleBtn.onclick = async () => { try { await api(`/v1/admin/connectors/${name}`, { body: { enabled: !c.enabled } }); d.close(); load(); } catch (e) { toast(e.message, "bad"); } };
    const keyBtn = c.pushed ? h("button", { class: "btn sm" }, icon("key"), c.push?.key_id ? "Rotate the push key" : "Make a push key") : null;
    if (keyBtn) keyBtn.onclick = async () => {
      if (!(await confirm({ title: "A new push key?", body: "The old key stops working now: the system that pushes needs the new one.", ok: "Make a new key" }))) return;
      try { const r = await api(`/v1/admin/connectors/${name}/key`, { body: {} }); showKey(c, r.push_secret); } catch (e) { toast(e.message, "bad"); }
    };
    const del = h("button", { class: "btn sm danger" }, icon("trash"), "Delete");
    del.onclick = async () => {
      if (!(await confirm({ title: `Delete ${name}?`, body: `Its config, cursor, secrets and push key go; the rows it wrote stay in ${c.target}, and its runs for their 30 days.`, ok: "Delete", danger: true, typed: name }))) return;
      try { await api(`/v1/admin/connectors/${name}`, { method: "DELETE" }); d.close(); toast(`${name} deleted`, "ok"); load(); } catch (e) { toast(e.message, "bad"); }
    };
    const editConf = h("textarea", { class: "input mono", rows: 10, spellcheck: "false", style: { width: "100%" } });
    editConf.value = pretty(c.config);
    const save = h("button", { class: "btn sm" }, icon("check"), "Save the config");
    save.onclick = async () => {
      let config; try { config = JSON.parse(editConf.value); } catch (e) { toast(`not JSON: ${e.message}`, "bad"); return; }
      try { await api(`/v1/admin/connectors/${name}`, { body: { config } }); toast("Saved", "ok"); load(); } catch (e) { toast(e.message, "bad"); }
    };
    const st = c.state || {};
    const runs = (c.runs || []).map((r) => h("tr", {},
      h("td", { class: "small", style: { whiteSpace: "nowrap" } }, r.started ? fmt.datetime(Date.parse(r.started)) : "–"),
      h("td", { class: "small" }, r.trigger),
      h("td", {}, !r.ok ? pill("failing", "failed") : r.warning ? pill("warn", "warning") : pill("ok", "ok")),
      h("td", { class: "num small" }, fmt.n(r.records || 0)), h("td", { class: "num small" }, fmt.n(r.rows_ || 0)),
      h("td", { class: "num small" }, took(r.took_ms)),
      h("td", { class: r.error ? "small error" : r.warning ? "small warn" : "small", style: { maxWidth: "300px", overflowWrap: "anywhere" } }, r.error || r.warning || (r.dropped?.length ? h("span", { class: "dim" }, `left out: ${r.dropped.join(", ")}`) : ""))));
    const facts = [
      ["Kind", kindTitle(c.kind)], ["Target", h("span", { class: "mono" }, tableLink(c.target))], ["Health", healthPill(c)], ["Schedule", every_text(c, v.kinds)],
      ["Runs", fmt.n(st.runs || 0)], ["Rows written", fmt.n(st.rows_total || 0)], ["Last ok", st.last_ok_ms ? fmt.ago(st.last_ok_ms) : "never"],
      ["Cursor", h("span", { class: "mono small", style: { wordBreak: "break-all" } }, (st.cursor || "–").slice(0, 120))],
      ["Secrets", c.secrets?.length ? h("span", { class: "mono" }, c.secrets.join(", ")) : "none"],
      ["Changed", c.updated_ms ? `${fmt.ago(c.updated_ms)}${c.updated_by ? ` by ${c.updated_by}` : ""}` : "–"],
      c.push ? ["Push to", h("span", { class: "mono small" }, `POST ${c.push.url}`)] : null,
    ].filter(Boolean);
    const d = drawer({ title: name, sub: `${kindTitle(c.kind)} into ${c.target}`, wide: true, body: h("div", { class: "stack", style: { gap: "14px" } },
      h("div", { class: "row", style: { gap: "6px", flexWrap: "wrap" } }, runBtn, dry, toggleBtn, keyBtn, del),
      c.invalid ? h("div", { class: "banner bad dt-banner" }, icon("bad"), h("div", { class: "small" }, `This config does not run: ${c.invalid}. Change it here, or delete it.`)) : null,
      st.last_error ? h("div", { class: "banner bad dt-banner" }, icon("bad"), h("div", { class: "small" }, st.last_error)) : null,
      !st.last_error && st.last_warning ? h("div", { class: "banner warn dt-banner" }, icon("warn"), h("div", { class: "small" }, `The last run went through, but: ${st.last_warning}`)) : null,
      out,
      h("table", { class: "t compact" }, h("tbody", {}, facts.map(([k, x]) => h("tr", {}, h("td", { class: "dim small", style: { width: "130px" } }, k), h("td", {}, x))))),
      card({ title: "Last runs", flush: true, body: runs.length ? h("div", { class: "tbl-wrap" }, h("table", { class: "t compact" }, h("thead", {}, h("tr", {}, ["started", "trigger", "result", "records", "rows", "took", "error"].map((x) => h("th", {}, x)))), h("tbody", {}, runs))) : empty("clock", "No runs yet") }),
      card({ title: "Config", body: h("div", { class: "stack", style: { gap: "6px" } }, editConf, h("div", { class: "row" }, save)) }),
      c.map ? card({ title: "Map in use", sub: c.config?.map ? "its own" : "the kind's or a preset for its target", body: h("pre", { class: "code small", style: { maxHeight: "260px", overflow: "auto" } }, pretty(c.map)) }) : null) });
  }

  function runReport(r) {
    return h("div", { class: "stack small", style: { gap: "4px" } },
      r.ok ? h("div", {}, pill("ok", "ok"), ` ${fmt.plural(r.records, "record")} read, ${fmt.plural(r.rows, "row")} written in ${took(r.took_ms)}`, r.more ? " · more to read: it runs again at once" : "")
        : h("div", { class: "error" }, r.error),
      r.warning ? h("div", { class: "warn" }, icon("warn"), " ", r.warning) : null,
      r.dropped?.length ? h("div", { class: "warn" }, "Left out (not columns of the target): ", h("span", { class: "mono" }, r.dropped.join(", "))) : null,
      r.skipped ? h("div", { class: "warn" }, `${fmt.plural(r.skipped, "record")} made no row: ${r.first_skip || ""}`) : null);
  }

  load();
  const stop = every(15000, () => { if (!page.isConnected) { stop(); return; } return load(); }, false);
}
