// Connections: the hub's book of other clusters (chloedb-server's
// crate::hub). Each connection is an address and an API key of that
// cluster, kept on this server (in the macOS Keychain or the Secret
// Service where there is one) and never sent to the browser; its command
// center opens at /c/<name>, where every call goes through this server,
// which adds the key. Adding one checks the address, the certificate (a
// self-signed one is shown by its SHA-256 and pinned once trusted) and the
// key (/v1/whoami with it).

import { h, icon, clear, fmt, api, toast, confirm, card, empty, failure, pill, copy, menu, connectionName, experimental } from "../core.js";

export default {
  id: "connections", title: "Connections", icon: "key", group: "Change",
  keywords: "connections hub clusters multi cluster book keys keychain remote cloud switch open another cluster certificate pin tls fingerprint",
  render,
};

const COLORS = ["#d95926", "#d55181", "#3987e5", "#199e70", "#9085e9", "#c98500", "#0f9fb5", "#a0732e"];
const short = (fp) => (fp ? fp.slice(0, 16).replace(/(..)(?=.)/g, "$1:") + "…" : "");

function render(page) {
  const list = h("div", { class: "stack" });
  const side = h("div", { class: "stack" });
  page.append(h("p", { class: "small dim", style: { margin: "0 0 12px" } }, experimental("The hub"), " Connections and the calls forwarded through them may change in any release."),
    h("div", { class: "conn-grid" }, list, side));
  let hub = null;
  async function load() {
    try { hub = await api("/v1/admin/hub", { hub: true }); }
    catch (e) { clear(list, failure(e, "The hub's book")); return; }
    draw();
  }
  function draw() {
    const cs = hub.connections || [];
    const rows = cs.map((c) => {
      const env = c.environment || c.cluster?.environment;
      const prod = env && /^prod/i.test(env);
      const cloud = c.kube ? (c.kube.provider === "aws" ? "EKS" : "DigitalOcean") : null;
      const cert = c.kube ? h("span", { class: "dim", title: "Calls go to the cluster's Kubernetes API with the cloud's credentials this server holds, then to its coordinators' Service: no public address, no kubectl." }, icon("lock"), ` the ${cloud} credentials here`)
        : c.pin ? h("span", { title: `Pinned: its certificate's SHA-256 is ${c.pin}` }, icon("lock"), " pinned ", h("span", { class: "mono small" }, short(c.pin)))
        : c.urls.some((u) => u.startsWith("https://")) ? h("span", { class: "dim" }, icon("lock"), " signed by an authority")
        : h("span", { class: c.allow_plain ? "warn" : "dim" }, icon(c.allow_plain ? "warn" : "unlock"), c.allow_plain ? " plain HTTP, allowed" : " this machine");
      const key = c.key || {};
      const expires = c.keyless ? "none: its Kubernetes API lets this hub in" : key.expires_ms ? (key.expires_ms < Date.now() ? h("span", { class: "error" }, "expired") : `expires ${fmt.ago(key.expires_ms).replace(" ago", "")} from now`) : key.root ? "the cluster's token" : "no expiry";
      const status = c.last_error ? pill("bad", "not answering") : c.last_ok_ms ? pill("ok", `answered ${fmt.ago(c.last_ok_ms)}`) : pill("info", "not called yet");
      return h("tr", {},
        h("td", {}, h("div", { class: "stack", style: { gap: "2px" } }, h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "conn-dot", style: { background: c.color } }), h("b", {}, c.name)), h("span", { class: "small dim" }, [c.cluster?.name, env].filter(Boolean).join(" · ") || "unnamed cluster", prod ? h("span", { class: "badge", style: { marginLeft: "6px" } }, "PROD") : null))),
        h("td", { class: "small" }, c.kube ? h("div", {}, "through ", h("span", { class: "mono" }, c.kube.cluster), `'s Kubernetes API (${c.kube.region})`) : h("div", { class: "mono" }, c.urls.join(", ")), cert),
        h("td", { class: "small", style: { whiteSpace: "nowrap" } }, h("div", {}, h("b", {}, c.keyless ? "open" : key.scope || "?"), key.name ? ` · ${key.name}` : ""), h("div", { class: "dim" }, expires)),
        h("td", { class: "small" }, status, c.last_error ? h("div", { class: "small error", style: { maxWidth: "280px" } }, c.last_error) : null),
        h("td", {}, h("div", { class: "row nw", style: { gap: "4px", justifyContent: "flex-end" } },
          h("a", { class: "btn sm primary", href: `/c/${c.name}#/home` }, "Open"),
          h("button", { class: "btn sm ghost", title: "More", "aria-label": `More for ${c.name}`, onclick: (e) => menu(e.currentTarget, [
            { icon: "refresh", label: "Check: ask it who its key is", onclick: () => check(c) },
            { icon: "key", label: "Put in a new key…", onclick: () => rekey(c) },
            "-",
            { icon: "trash", label: "Take it out of the book…", danger: true, onclick: () => remove(c) },
          ]) }, icon("dots")))));
    });
    clear(list, card({
      title: "Connections", sub: `${fmt.plural(cs.length, "cluster")} this command center reaches`,
      tools: connectionName ? h("a", { class: "btn sm", href: "/#/connections" }, "Open on the hub") : null,
      flush: true,
      body: cs.length ? h("div", { class: "tbl-wrap" }, h("table", { class: "t" }, h("thead", {}, h("tr", {}, h("th", {}, "connection"), h("th", {}, "address"), h("th", {}, "key"), h("th", {}, "last call"), h("th", {}, ""))), h("tbody", {}, rows)))
        : empty("cluster", "No other clusters yet", "Add one with its address and an API key of it: its command center then opens here, and the key stays on this server."),
    }));
    drawSide();
  }
  function drawSide() {
    const name = h("input", { class: "input mono", placeholder: "prod-search", autocomplete: "off", spellcheck: "false" });
    const url = h("input", { class: "input mono", placeholder: "https://chloedb.example.com", autocomplete: "off", spellcheck: "false" });
    const key = h("input", { class: "input mono", type: "password", placeholder: "sk_…", autocomplete: "off" });
    let color = COLORS[(hub.connections || []).length % COLORS.length];
    const swatches = h("div", { class: "row", style: { gap: "6px" } });
    const drawSwatches = () => clear(swatches, COLORS.map((c) => h("button", { class: "conn-dot lg" + (c === color ? " on" : ""), title: c, "aria-label": `Color ${c}`, style: { background: c }, onclick: () => { color = c; drawSwatches(); } })));
    drawSwatches();
    const note = h("div");
    const go = h("button", { class: "btn primary" }, icon("plus"), "Add the connection");
    const st = { trust: null, allow_plain: false };
    go.onclick = async () => {
      go.disabled = true;
      clear(note);
      const body = { name: name.value.trim(), url: url.value.trim(), key: key.value.trim(), color, ...(st.trust ? { trust: st.trust } : {}), ...(st.allow_plain ? { allow_plain: true } : {}) };
      try {
        const c = await api("/v1/admin/hub/connections", { body, hub: true });
        toast(`${c.name} added: its command center opens at /c/${c.name}`, "ok");
        st.trust = null; st.allow_plain = false;
        await load();
      } catch (e) {
        go.disabled = false;
        if (e.body?.needs_trust) {
          clear(note, h("div", { class: "banner warn dt-banner" }, icon("warn"), h("div", { class: "grow stack", style: { gap: "6px" } },
            h("b", {}, "Its certificate is self-signed"),
            h("div", { class: "small" }, "Compare this SHA-256 with the one the cluster shows (its operator's logs, or ", h("span", { class: "mono" }, "openssl x509 -fingerprint -sha256"), " on its certificate). Trusting it pins it: a later certificate that differs is refused."),
            h("div", { class: "mono small", style: { wordBreak: "break-all" } }, e.body.fingerprint),
            h("div", { class: "row", style: { gap: "6px" } }, h("button", { class: "btn sm primary", onclick: () => { st.trust = e.body.fingerprint; go.onclick(); } }, "Trust it and add"), h("button", { class: "btn sm", onclick: () => copy(e.body.fingerprint, "Fingerprint copied") }, icon("copy"), "Copy")))));
        } else if (/plain HTTP beyond this machine/.test(e.message)) {
          clear(note, h("div", { class: "banner warn dt-banner" }, icon("warn"), h("div", { class: "grow stack", style: { gap: "6px" } },
            h("b", {}, "Plain HTTP beyond this machine"), h("div", { class: "small" }, "The key and every row would cross the network readable. Give the cluster HTTPS if you can."),
            h("button", { class: "btn sm danger", onclick: () => { st.allow_plain = true; go.onclick(); } }, "Allow plain HTTP for this one"))));
        } else clear(note, h("div", { class: "error small" }, e.message));
      }
    };
    const keys = hub.keys || {};
    clear(side,
      (hub.connections || []).length ? copyCard() : null,
      card({ title: "Add a connection", body: h("div", { class: "stack", style: { gap: "10px" } },
        h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "Name, as /c/<name> shows it"), name),
        h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "Address: a coordinator's URL (more, comma-separated)"), url),
        h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "An API key of that cluster (Access and audit there makes one)"), key),
        h("div", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "Color, on its top bar and confirms"), swatches),
        note, h("div", { class: "row" }, go)) }),
      card({ title: "Where the keys are", body: h("div", { class: "stack small", style: { gap: "6px" } },
        h("div", {}, icon("lock"), " ", h("b", {}, keys.what || "?")),
        h("div", { class: "dim" }, "The browser never holds a cluster's key: this server adds it to each call it forwards, and names itself in the call (", h("span", { class: "mono" }, `x-strata-via: hub ${hub.device}`), ")."),
        h("div", { class: "dim" }, "Taking a connection out removes its key from here; the key still works on its cluster until it expires or is revoked there."),
        hub.file ? h("div", { class: "dim" }, "The book (no keys in it): ", h("span", { class: "mono" }, hub.file)) : null) }));
  }
  /** One-off copies of tables (crate::copy): from this server or a connection that is one server, to another end. */
  function copyCard() {
    const ends = [["", "this server"], ...(hub.connections || []).map((c) => [c.name, c.name])];
    const st = { from: "", to: hub.connections[0].name, tables: null, picked: new Set(), busy: false, out: null };
    const fromSel = h("select", { class: "select sm", "aria-label": "Copy from", onchange: () => { st.from = fromSel.value; st.out = null; loadTables(); } }, ends.map(([v, l]) => h("option", { value: v }, l)));
    const toSel = h("select", { class: "select sm", "aria-label": "Copy to", onchange: () => { st.to = toSel.value; st.out = null; draw(); } }, ends.map(([v, l]) => h("option", { value: v, selected: v === st.to }, l)));
    const list = h("div", { class: "stack", style: { gap: "4px" } });
    const go = h("button", { class: "btn primary" }, icon("copy"), "Copy");
    const out = h("div");
    const draw = () => {
      clear(list, st.tables == null ? h("span", { class: "small dim" }, "Asking for its tables…")
        : st.tables.error ? h("span", { class: "small error" }, st.tables.error)
        : !st.tables.length ? h("span", { class: "small dim" }, "No tables there.")
        : st.tables.map((t) => h("label", { class: "row nw small" }, h("input", { type: "checkbox", checked: st.picked.has(t), onchange: (e) => { e.target.checked ? st.picked.add(t) : st.picked.delete(t); draw(); } }), h("span", { class: "mono" }, t))));
      go.disabled = st.busy || !st.picked.size || st.from === st.to;
      go.lastChild.textContent = st.busy ? "Copying…" : st.picked.size ? `Copy ${fmt.plural(st.picked.size, "table")}` : "Copy";
      clear(out, st.from === st.to ? h("div", { class: "small error" }, "From and to are the same.") : st.out);
    };
    async function loadTables() {
      st.tables = null; st.picked = new Set(); draw();
      try { st.tables = ((await api(st.from ? `/c/${st.from}/v1/tables` : "/v1/tables", { hub: true })).tables || []).map((t) => t.table); }
      catch (e) { st.tables = { error: e.message }; }
      draw();
    }
    go.onclick = async () => {
      const tables = [...st.picked], to = st.to || "this server", from = st.from || "this server";
      const ok = await confirm({ title: `Copy ${tables.join(", ")} to ${to}?`, ok: "Copy", body: h("p", { class: "dim", style: { margin: 0 } }, `${to} gets each table with its schema (its shard and cluster keys too) if it has none of that name, and every row of ${from}'s by its key: a row with the same key there is replaced. Later writes are not followed.`) });
      if (!ok) return;
      st.busy = true; st.out = null; draw();
      try {
        const j = await api("/v1/admin/hub/copy", { body: { from: st.from, to: st.to, tables }, hub: true });
        st.out = h("div", { class: "small" }, icon("check"), ` ${fmt.n(j.written || 0)} rows of ${tables.join(", ")} copied to ${to} in ${fmt.ms(j.took_ms)}.`);
        toast(`Copied to ${to}`, "ok");
      } catch (e) { st.out = h("div", { class: "small error" }, e.message); }
      st.busy = false; draw();
    };
    loadTables();
    return card({ title: "Copy tables", sub: "once, between this server and a connection", body: h("div", { class: "stack", style: { gap: "10px" } },
      h("div", { class: "row", style: { gap: "8px", flexWrap: "wrap" } }, h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "From"), fromSel), h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "To"), toSel)),
      list,
      h("div", { class: "small dim" }, "From this server or a connection that is one server: a cluster's coordinators hold no rows to send."),
      h("div", { class: "row" }, go), out) });
  }

  async function check(c) {
    try { const x = await api(`/v1/admin/hub/connections/${c.name}/check`, { method: "POST", body: {}, hub: true }); toast(`${x.name} answered: ${x.key?.scope || "?"} key`, "ok"); }
    catch (e) { toast(`${c.name}: ${e.message}`, "bad", 9000); }
    load();
  }
  async function rekey(c) {
    const inp = h("input", { class: "input mono", type: "password", placeholder: "sk_…", autocomplete: "off", style: { width: "100%" } });
    const ok = await confirm({ title: `A new key for ${c.name}`, ok: "Check it and keep it", body: h("div", { class: "stack", style: { gap: "8px" } }, h("p", { class: "dim", style: { margin: 0 } }, "The key is checked with the cluster first, then it replaces the one kept here."), inp) });
    if (!ok) return;
    try { await api(`/v1/admin/hub/connections/${c.name}`, { body: { key: inp.value.trim() }, hub: true }); toast(`${c.name}'s key replaced`, "ok"); load(); }
    catch (e) { toast(e.message, "bad", 9000); }
  }
  async function remove(c) {
    const ok = await confirm({ title: `Take ${c.name} out of the book?`, ok: "Take it out", danger: true, body: h("p", { class: "dim", style: { margin: 0 } }, `Its key is removed from this server. The key still works on ${c.name} until it expires or is revoked there (Access and audit, on ${c.name}).`) });
    if (!ok) return;
    try { await api(`/v1/admin/hub/connections/${c.name}`, { method: "DELETE", hub: true }); load(); }
    catch (e) { toast(e.message, "bad", 9000); }
  }
  load();
}
