// Cloud: a Strata cluster on Amazon EKS or DigitalOcean Kubernetes (DOKS),
// planned from this page. The account's key is kept on this server (never
// shown again, every change audited); the region's machines are drawn by
// price and memory with their live prices and the account's limit; the
// plan lays the shards' copies on the nodes, prices them line by line,
// says what would stop them, and hands over what makes the cluster (a
// CloudFormation template, or DOKS's request) and the Kubernetes
// manifests, checked by the cloud itself. **Create it** makes the cluster
// from here, after its name is typed again: this server asks the cloud,
// installs Strata's operator through the new cluster's API, follows its
// copies until they answer, and deletes it (its volumes first) when asked.

import { h, s, icon, clear, fmt, card, api, toast, confirm, seg, toggle, pill, empty, table, tipOn, hideTip, debounce, copy, failure, clamp, modal } from "../core.js";

export default {
  id: "cloud", title: "Cloud", icon: "cloud", group: "Admin",
  keywords: "cloud aws amazon eks digitalocean doks droplet kubernetes k8s credentials access key token region instance instances machine machines size sizes ec2 graviton price prices cost spot quota limit cloudformation template deploy",
  render, onParams,
};

let switchProvider = null;
function onParams(r) { switchProvider?.(r.params.get("provider")); }

const KINDS = [
  ["general", "General", "var(--s1)"],
  ["memory", "Memory", "var(--s3)"],
  ["compute", "Compute", "var(--s2)"],
  ["storage", "Local disk", "var(--s4)"],
  ["burstable", "Burstable", "var(--s7)"],
  ["shared", "Shared CPU", "var(--s7)"],
];
const kindColor = (k) => (KINDS.find((x) => x[0] === k) || KINDS[0])[2];
const kindLabel = (k) => (KINDS.find((x) => x[0] === k) || [k, k])[1];
const SHARD = (i) => `var(--s${(i % 8) + 1})`;
const usd = (v, d) => (v == null ? "–" : "$" + (d != null ? v.toFixed(d) : v >= 100 ? fmt.n(v, 0) : v >= 10 ? v.toFixed(1) : v >= 1 ? v.toFixed(2) : v.toFixed(3)));
const gib = (v) => (v == null ? "–" : v >= 1024 ? fmt.n(v / 1024, 1) + " TiB" : (Number.isInteger(v) ? v : v.toFixed(1)) + " GiB");

/** The read-only permissions the AWS side of this page needs. */
const POLICY = {
  Version: "2012-10-17",
  Statement: [{
    Effect: "Allow",
    Action: ["sts:GetCallerIdentity", "ec2:DescribeRegions", "ec2:DescribeInstanceTypes", "ec2:DescribeSpotPriceHistory", "pricing:GetProducts", "servicequotas:GetServiceQuota", "cloudformation:ValidateTemplate"],
    Resource: "*",
  }],
};

/** What differs between the clouds. */
const PROVIDERS = {
  aws: {
    name: "AWS", k8s: "Amazon EKS", base: "/v1/admin/aws", keyPath: "/credentials", status: (st) => st?.credentials,
    regions: ["us-east-1", "us-east-2", "us-west-2", "eu-west-1", "eu-central-1", "eu-north-1", "ap-northeast-1", "ap-southeast-1", "ap-southeast-2", "ap-south-1", "ca-central-1", "sa-east-1"],
    region: "us-east-1", spot: true, cpus: [["any", "Any CPU"], ["arm64", "Graviton"], ["x86_64", "x86-64"]],
    cpu: (m) => (m.arch === "arm64" ? "arm64" : "x86_64"), filled: (m) => m.arch === "arm64", cpuLabel: (m) => (m.arch === "arm64" ? "Graviton" : "x86-64"),
    hollow: "hollow: x86-64", ringed: "the newest Graviton general and memory machines", hoursAMonth: 730,
    pick: (cat) => cat.recommended?.find((t) => t.startsWith("m")) || cat.recommended?.[0],
    check: "Check with AWS", checkTitle: "CloudFormation's own check of the template: free, and makes nothing",
  },
  digitalocean: {
    name: "DigitalOcean", k8s: "DigitalOcean Kubernetes (DOKS)", base: "/v1/admin/digitalocean", keyPath: "/token", status: (st) => st?.token,
    regions: ["nyc3", "nyc1", "sfo3", "sfo2", "ams3", "lon1", "fra1", "tor1", "sgp1", "blr1", "syd1", "atl1"],
    region: "nyc3", spot: false, cpus: [["any", "Any CPU"], ["regular", "Regular"], ["intel", "Premium Intel"], ["amd", "Premium AMD"]],
    cpu: (m) => (m.family.endsWith("-intel") ? "intel" : m.family.endsWith("-amd") ? "amd" : "regular"), filled: (m) => !m.burstable, cpuLabel: (m) => (m.family.endsWith("-intel") ? "Premium Intel" : m.family.endsWith("-amd") ? "Premium AMD" : m.burstable ? "Shared" : "Dedicated"),
    hollow: "hollow: shared CPUs", ringed: "dedicated general and memory-optimized sizes of 2 to 8 vCPUs", hoursAMonth: 672,
    pick: (cat) => cat.recommended?.find((t) => t.startsWith("m-4vcpu")) || cat.recommended?.[0],
    check: "Check with DigitalOcean", checkTitle: "DigitalOcean's own answer, fetched now: the account, the region, the size, the version and the droplet limit. Makes nothing",
  },
};

function download(name, text, type = "application/json") {
  const a = h("a", { href: URL.createObjectURL(new Blob([text], { type })), download: name });
  document.body.append(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

/** Commands, each with a copy button. */
const cmds = (list) => h("div", { class: "cloud-cmds" }, list.map((c) => h("div", { class: "row nw" }, h("code", { class: "mono small" }, c), h("button", { class: "btn xs ghost", title: "Copy", onclick: () => copy(c) }, icon("copy")))));

/** Asks before something that cannot be taken back, with `name` typed again. */
function typed({ title, body, name, ok }) {
  return new Promise((resolve) => {
    let answered = false;
    const inp = h("input", { class: "input mono", placeholder: name, autocomplete: "off", spellcheck: "false", "aria-label": "the cluster's name" });
    const yes = h("button", { class: "btn danger solid", disabled: true }, ok);
    const no = h("button", { class: "btn" }, "Cancel");
    inp.oninput = () => { yes.disabled = inp.value.trim() !== name; };
    inp.onkeydown = (e) => { if (e.key === "Enter" && !yes.disabled) yes.click(); };
    const m = modal({
      title,
      body: h("div", { class: "stack" }, body, h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "Type ", h("b", { class: "mono" }, name), " to go on"), inp)),
      foot: [no, yes], onClose: () => { if (!answered) resolve(false); },
    });
    yes.onclick = () => { answered = true; m.close(); resolve(true); };
    no.onclick = () => { answered = true; m.close(); resolve(false); };
  });
}

/** Where a made cluster is, as a pill. */
const PHASES = {
  creating: ["pending", "creating"], installing: ["pending", "installing Strata"], starting: ["pending", "starting its copies"],
  ready: ["ok", "ready"], deleting: ["warn", "deleting"], deleted: ["disabled", "deleted"], failed: ["bad", "failed"],
};
const ON_ITS_WAY = new Set(["creating", "installing", "starting", "deleting"]);

function stepper(value, min, max, onChange, label, unit = "") {
  const v = h("b", { class: "num" }, String(value) + unit);
  const set = (x) => { x = clamp(x, min, max); if (x !== value) { value = x; v.textContent = String(x) + unit; onChange(x); } };
  return h("div", { class: "cloud-step" },
    h("span", { class: "small dim" }, label),
    h("div", { class: "row nw" },
      h("button", { class: "btn xs", "aria-label": "fewer " + label, disabled: value <= min, onclick: () => set(value - (value > 20 && unit ? 10 : 1)) }, icon("minus")),
      v,
      h("button", { class: "btn xs", "aria-label": "more " + label, disabled: value >= max, onclick: () => set(value + (value >= 20 && unit ? 10 : 1)) }, icon("plus"))));
}

const remembered = () => { try { return localStorage.getItem("strata.cloud") || "aws"; } catch { return "aws"; } };

function render(page, r) {
  let prov = PROVIDERS[r?.params?.get("provider")] ? r.params.get("provider") : remembered();
  let P = PROVIDERS[prov];
  const head = h("div"), made = h("div"), creds = h("div"), where = h("div"), machines = h("div"), planEl = h("div");
  page.append(head, made, h("div", { class: "cloud-top" }, creds, where), machines, planEl);
  let clusters = [], pollT = null, alive = true, creating = false;
  const createEl = h("div");

  let status, regions, region, cat, catErr, loadingCat, editing, filt, form, plan, planErr, checked, checking;
  let seq = 0;
  function reset() {
    status = null; regions = null; region = null; cat = null; catErr = null; loadingCat = false; editing = false;
    filt = { kind: "all", cpu: "any", spot: false, q: "" };
    form = { name: "search", machine: null, nodes: 3, spot: false, ha: false, version: "", volume_gb: 10, shards: 2, replicas: 3, coordinators: 1, image: "" };
    plan = null; planErr = null; checked = null; checking = false; seq++;
  }
  reset();

  const key = () => P.status(status);
  const have = () => !!key();

  switchProvider = (p) => {
    if (!PROVIDERS[p] || p === prov) return;
    prov = p; P = PROVIDERS[p];
    try { localStorage.setItem("strata.cloud", p); } catch { /* private mode */ }
    reset(); clear(creds); clear(where); clear(machines); clear(planEl); load();
  };

  async function load() {
    const mine = seq;
    drawHead();
    try { status = await api(P.base); } catch (e) { if (mine !== seq) return; clear(creds, failure(e, "The cloud view")); return; }
    if (mine !== seq) return;
    drawHead(); drawCreds();
    if (have()) {
      region = region || key().region || P.region;
      if (!regions) api(P.base + "/regions").then((x) => { if (mine === seq) { regions = x.regions; drawWhere(); } }).catch(() => { regions = null; });
      await loadCatalog(false);
    } else { cat = null; drawWhere(); drawMachines(); drawPlan(); }
  }

  async function loadCatalog(fresh) {
    const mine = seq;
    loadingCat = true; catErr = null; drawWhere(); drawMachines();
    try {
      const c = await api(`${P.base}/machines?region=${encodeURIComponent(region)}${fresh ? "&fresh=1" : ""}`);
      if (mine !== seq) return;
      cat = c;
      if (!form.machine || !cat.machines.some((m) => m.type === form.machine)) form.machine = P.pick(cat) || cat.machines[0]?.type || null;
      if (form.version && !(cat.versions || []).includes(form.version)) form.version = "";
    } catch (e) { if (mine !== seq) return; catErr = e; cat = null; }
    loadingCat = false;
    drawWhere(); drawMachines(); replan();
  }

  // ---------------------------------------------------------- the header
  function drawHead() {
    const k = key();
    const label = !status ? null : k ? pill("ok", prov === "aws" ? (k.account ? `account ${k.account}` : "keys from the environment") : (k.account?.team ? `team ${k.account.team}` : "token from the environment")) : pill("pending", `no ${P.name} key`);
    clear(head, h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, "Cloud"), h("p", {}, `Plan a Strata cluster on ${P.k8s}: machines with their prices, where the copies go, what it costs, and what makes it.`)),
      h("div", { class: "tools" },
        seg(Object.entries(PROVIDERS).map(([k, v]) => [k, v.name]), prov, (v) => { location.hash = "#/cloud?provider=" + v; }, { "aria-label": "Cloud" }),
        label)));
  }

  // ---------------------------------------------------------- the key
  function drawCreds() {
    const k = key();
    if (k && !editing) {
      const kept = status.file ? [h("dt", {}, "Kept in"), h("dd", { class: "mono small" }, status.file)] : [h("dt", {}, "Kept in"), h("dd", { class: "small" }, "memory only: this coordinator forgets it when it stops")];
      const rows = prov === "aws" ? [
        h("dt", {}, "Access key"), h("dd", { class: "mono" }, k.access_key_id, k.session_token ? h("span", { class: "small dim" }, " · with a session token") : null),
        k.arn ? [h("dt", {}, "Who"), h("dd", { class: "mono small" }, k.arn)] : null,
        h("dt", {}, "Region"), h("dd", {}, k.region),
      ] : [
        h("dt", {}, "Token"), h("dd", { class: "mono" }, k.token),
        k.account ? [h("dt", {}, "Team"), h("dd", {}, k.account.team || "–", h("span", { class: "small dim" }, ` · ${k.account.status} · ${fmt.n(k.account.droplet_limit)} droplets allowed`))] : null,
      ];
      clear(creds, card({
        title: prov === "aws" ? "Credentials" : "Token", sub: k.source === "stored" ? "kept on this server" : "from this server's environment",
        tools: k.source === "stored" ? [
          h("button", { class: "btn sm", onclick: () => { editing = true; drawCreds(); } }, icon("edit"), "Replace"),
          h("button", { class: "btn sm danger", onclick: forget }, icon("trash"), "Forget"),
        ] : [h("button", { class: "btn sm", onclick: () => { editing = true; drawCreds(); } }, icon("plus"), "Save one here")],
        body: h("div", { class: "stack" },
          h("dl", { class: "kv" }, rows, k.saved_ms ? [h("dt", {}, "Saved"), h("dd", {}, `${fmt.ago(k.saved_ms)} by ${k.by}`)] : null, kept),
          h("div", { class: "small dim" }, "The secret never leaves this server: no answer of its API includes it. Saving and forgetting it are in the audit log.")),
      }));
      return;
    }
    const err = h("div", { class: "small", style: { color: "var(--crit-ink)" } });
    const save = h("button", { class: "btn primary", type: "submit" }, icon("lock"), "Save and check");
    const cancel = editing ? h("button", { class: "btn", type: "button", onclick: () => { editing = false; drawCreds(); } }, "Cancel") : null;
    const saved = async (r, msg) => { toast(msg, "ok"); editing = false; regions = null; cat = null; region = r; await load(); };
    let f, first, note, extra = null;
    if (prov === "aws") {
      const id = first = h("input", { class: "input mono", placeholder: "AKIA…", autocomplete: "off", spellcheck: "false" });
      const secret = h("input", { class: "input mono", type: "password", placeholder: "secret access key", autocomplete: "new-password" });
      const token = h("input", { class: "input mono", type: "password", placeholder: "session token (temporary keys only)", autocomplete: "off" });
      const reg = h("select", { class: "select" }, P.regions.map((x) => h("option", { value: x, selected: x === (key()?.region || P.region) }, x)));
      f = h("form", { class: "stack cloud-form", onsubmit: async (e) => {
        e.preventDefault(); save.disabled = true; err.textContent = "";
        try {
          const body = { access_key_id: id.value.trim(), secret_access_key: secret.value.trim(), region: reg.value };
          if (token.value.trim()) body.session_token = token.value.trim();
          const res = await api(P.base + P.keyPath, { method: "PUT", body });
          secret.value = ""; token.value = "";
          await saved(res.credentials?.region || body.region, `Saved: ${res.identity?.arn || "the keys"}`);
        } catch (ex) { err.textContent = ex.message; }
        save.disabled = false;
      } },
        h("label", { class: "stack cloud-field" }, h("span", { class: "small dim" }, "Access key id"), id),
        h("label", { class: "stack cloud-field" }, h("span", { class: "small dim" }, "Secret access key"), secret),
        h("details", {}, h("summary", { class: "small dim" }, "Temporary keys (STS)"), h("div", { style: { marginTop: "6px" } }, token)),
        h("label", { class: "stack cloud-field" }, h("span", { class: "small dim" }, "Region"), reg),
        err, h("div", { class: "row" }, save, cancel));
      note = "AWS checks them before they are kept (STS GetCallerIdentity). They stay on this server, readable by its user only, and are never answered back; saving and forgetting them go to the audit log.";
      extra = h("details", {}, h("summary", { class: "small" }, "What this page needs (read only)"),
        h("pre", { class: "code small", style: { marginTop: "6px" } }, JSON.stringify(POLICY, null, 2)),
        h("button", { class: "btn xs", onclick: () => copy(JSON.stringify(POLICY, null, 2), "Policy copied") }, icon("copy"), "Copy the policy"),
        h("div", { class: "small dim", style: { marginTop: "6px" } }, "Making the cluster from the template needs more: VPCs, IAM roles and EKS clusters, usually an administrator's."));
    } else {
      const tok = first = h("input", { class: "input mono", type: "password", placeholder: "dop_v1_…", autocomplete: "new-password", spellcheck: "false" });
      f = h("form", { class: "stack cloud-form", onsubmit: async (e) => {
        e.preventDefault(); save.disabled = true; err.textContent = "";
        try {
          const res = await api(P.base + P.keyPath, { method: "PUT", body: { token: tok.value.trim() } });
          tok.value = "";
          await saved(region || P.region, `Saved: the token of ${res.account?.team || "the account"}`);
        } catch (ex) { err.textContent = ex.message; }
        save.disabled = false;
      } },
        h("label", { class: "stack cloud-field" }, h("span", { class: "small dim" }, "Personal access token"), tok),
        err, h("div", { class: "row" }, save, cancel));
      note = "DigitalOcean checks it before it is kept (GET /v2/account). It stays on this server, readable by its user only, and is never answered back; saving and forgetting it go to the audit log.";
      extra = h("div", { class: "small dim" }, "A token with read scopes is enough for this page. Making the cluster needs one that may create Kubernetes clusters (", h("a", { href: "https://cloud.digitalocean.com/account/api/tokens", target: "_blank", rel: "noopener" }, "API tokens"), ").");
    }
    clear(creds, card({
      title: prov === "aws" ? "Credentials" : "Token", sub: prov === "aws" ? "an IAM user's access key" : "a personal access token",
      body: h("div", { class: "stack" }, f, h("div", { class: "small dim" }, note), extra),
    }));
    setTimeout(() => first.focus(), 0);
  }

  async function forget() {
    const what = prov === "aws" ? "the AWS keys" : "the DigitalOcean token";
    if (!(await confirm({ title: `Forget ${what}?`, body: `This server deletes ${prov === "aws" ? "them" : "it"}. Nothing in the ${P.name} account changes.`, ok: "Forget", danger: true }))) return;
    try { await api(P.base + P.keyPath, { method: "DELETE" }); toast(`Forgot ${what}`, "ok"); cat = null; plan = null; await load(); } catch (e) { toast(e.message, "bad"); }
  }

  // ---------------------------------------------------------- region and limits
  function meter(label, have, need, unit, bad, more) {
    const over = need != null && have != null && need > have;
    const zero = have === 0;
    return h("div", { class: "cloud-quota" + (zero || over || bad ? " bad" : "") },
      h("div", { class: "row between" }, h("span", {}, label), h("b", { class: "num" }, have == null ? "unknown" : `${fmt.n(have)} ${unit}`)),
      h("div", { class: "barbox" }, h("div", { class: "bar", style: { width: have ? clamp((need || 0) / have * 100, 0, 100) + "%" : "100%", background: zero || over || bad ? "var(--crit)" : "var(--s1)" } })),
      more);
  }

  function drawWhere() {
    if (!have()) { clear(where, card({ title: "Region and limits", body: empty("cloud", `No ${P.name} key yet`, prov === "aws" ? "Save an access key to see the machines of a region, their prices and your account's quota." : "Save a token to see the sizes of a region, their prices and your account's droplet limit.") })); return; }
    const list = (regions || P.regions).map((x) => (typeof x === "string" ? { slug: x, name: x } : x));
    const sel = h("select", { class: "select", onchange: () => { region = sel.value; plan = null; checked = null; loadCatalog(false); } }, list.map((x) => h("option", { value: x.slug, selected: x.slug === region }, x.name === x.slug ? x.slug : `${x.name} (${x.slug})`)));
    const need = plan?.quota?.needs;
    let meters = null, prices = null;
    if (cat && prov === "aws") {
      const quotaLink = (spot) => h("a", { href: `https://${region}.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/${spot ? "L-34B43A08" : "L-1216C47A"}`, target: "_blank", rel: "noopener" }, "Ask AWS for more");
      const m = (label, have, spot) => meter(label, have, spot === form.spot ? need : 0, "vCPUs", false,
        have === 0 ? h("div", { class: "small" }, "The account may start no instances of this kind here. ", quotaLink(spot), ", with a use case: new accounts are often refused without one.")
          : need != null && spot === form.spot && need > have ? h("div", { class: "small" }, `The plan needs ${need}. `, quotaLink(spot)) : null);
      meters = [m("On-demand standard instances", cat.quota_vcpus, false), m("Spot standard instances", cat.spot_quota_vcpus, true)];
      prices = `${fmt.n(cat.machines.length)} machines, prices from AWS's price list ${fmt.ago(cat.at_ms)}. EKS ${usd(cat.eks_hour)}/h a cluster, gp3 ${usd(cat.gp3_gb_month, 3)} a GB-month, a public IPv4 address ${usd(cat.ipv4_hour)}/h.`;
    } else if (cat) {
      const free = cat.droplet_limit != null && cat.droplets != null ? cat.droplet_limit - cat.droplets : null;
      meters = [meter("Droplets", cat.droplet_limit, (cat.droplets || 0) + (need || 0), "allowed", false,
        h("div", { class: "small dim" }, cat.droplets == null ? "" : `${fmt.n(cat.droplets)} running${need ? `, the plan adds ${need}` : ""}${free != null && need > free ? ". " : ""}`,
          free != null && need > free ? h("a", { href: "https://cloud.digitalocean.com/account/team/droplet_limit_increase", target: "_blank", rel: "noopener" }, "Ask for more") : null))];
      prices = `${fmt.n(cat.machines.length)} sizes DOKS takes here, prices from DigitalOcean ${fmt.ago(cat.at_ms)}. The control plane is free (${usd(cat.ha_month, 0)} a month highly available), volumes ${usd(cat.volume_gb_month, 2)} a GiB-month. Kubernetes ${(cat.versions || []).slice(0, 3).join(", ")}.`;
    }
    clear(where, card({
      title: "Region and limits",
      tools: [h("button", { class: "btn sm ghost", title: "Fetch the machines and prices again", disabled: loadingCat, onclick: () => loadCatalog(true) }, icon("refresh"))],
      body: h("div", { class: "stack" },
        h("label", { class: "row" }, h("span", { class: "small dim" }, "Region"), sel),
        loadingCat ? h("div", { class: "small dim" }, `Fetching the machines, prices and limits from ${P.name}…`) : null,
        meters,
        cat?.missing?.length ? h("div", { class: "small dim" }, "Not fetched: " + cat.missing.join("; ")) : null,
        prices ? h("div", { class: "small dim" }, prices) : null),
    }));
  }

  // ---------------------------------------------------------- machines
  const price = (m) => (filt.spot ? m.spot_hour : m.usd_hour);
  const shown = () => (cat?.machines || []).filter((m) =>
    (filt.kind === "all" || m.kind === filt.kind) &&
    (filt.cpu === "any" || P.cpu(m) === filt.cpu) &&
    (!filt.q || m.type.includes(filt.q.toLowerCase()) || (m.processor || "").toLowerCase().includes(filt.q.toLowerCase())) &&
    price(m) != null);
  const kinds = () => KINDS.filter(([k]) => (cat?.machines || []).some((m) => m.kind === k));

  function select(t) { form.machine = t; drawMachines(); replan(); }

  function chart(list) {
    const W = 760, H = 300, L = 48, B = 28, T = 10, R = 12;
    const xs = list.map(price).filter((v) => v > 0), ys = list.map((m) => m.memory_gib).filter((v) => v > 0);
    if (!xs.length) return empty("info", "No machine matches");
    const lx = [Math.log10(Math.min(...xs)) - 0.05, Math.log10(Math.max(...xs)) + 0.05];
    const ly = [Math.log10(Math.min(...ys)) - 0.05, Math.log10(Math.max(...ys)) + 0.05];
    const X = (v) => L + (Math.log10(v) - lx[0]) / (lx[1] - lx[0]) * (W - L - R);
    const Y = (v) => H - B - (Math.log10(v) - ly[0]) / (ly[1] - ly[0]) * (H - B - T);
    const g = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "cloud-chart", role: "img", "aria-label": "Machines by price an hour and memory" });
    for (let e = Math.ceil(lx[0]); e <= Math.floor(lx[1]); e++) for (const k of [1, 2, 5]) {
      const v = k * 10 ** e; if (Math.log10(v) < lx[0] || Math.log10(v) > lx[1]) continue;
      g.append(s("line", { x1: X(v), x2: X(v), y1: T, y2: H - B, class: "cloud-grid" }), s("text", { x: X(v), y: H - B + 16, class: "cloud-ax", "text-anchor": "middle" }, usd(v)));
    }
    for (let e = Math.ceil(ly[0]); e <= Math.floor(ly[1]); e++) for (const k of [1, 4]) {
      const vv = Math.round(k * 2 ** (e * Math.log2(10))); if (vv <= 0) continue;
      if (Math.log10(vv) < ly[0] || Math.log10(vv) > ly[1]) continue;
      g.append(s("line", { x1: L, x2: W - R, y1: Y(vv), y2: Y(vv), class: "cloud-grid" }), s("text", { x: L - 6, y: Y(vv) + 4, class: "cloud-ax", "text-anchor": "end" }, gib(vv)));
    }
    g.append(s("text", { x: W - R, y: H - 4, class: "cloud-ax", "text-anchor": "end" }, filt.spot ? "spot price an hour →" : "price an hour →"), s("text", { x: 4, y: T + 4, class: "cloud-ax" }, "↑ memory"));
    const rec = new Set(cat?.recommended || []);
    // The chosen machine drawn last, over the others.
    const order = list.slice().sort((a, b) => (a.type === form.machine) - (b.type === form.machine));
    for (const m of order) {
      const p = price(m); if (!(p > 0)) continue;
      const on = m.type === form.machine;
      const c = s("circle", {
        cx: X(p), cy: Y(m.memory_gib), r: on ? 7 : 4,
        class: "cloud-dot" + (on ? " on" : ""), tabindex: "0", role: "button", "aria-label": `${m.type}: ${m.vcpu} vCPUs, ${gib(m.memory_gib)}, ${usd(p)} an hour`,
        style: { fill: P.filled(m) ? kindColor(m.kind) : "var(--surface)", stroke: kindColor(m.kind) },
        onclick: () => select(m.type), onkeydown: (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); select(m.type); } },
      });
      g.append(tipOn(c, () => tip(m)));
      if (rec.has(m.type) && !on) g.append(s("circle", { cx: X(p), cy: Y(m.memory_gib), r: 7.5, class: "cloud-rec" }));
    }
    return g;
  }

  function tip(m) {
    return h("div", { class: "stack", style: { gap: "2px" } },
      h("b", {}, m.type, " · ", kindLabel(m.kind)),
      h("div", {}, `${m.vcpu} vCPUs · ${gib(m.memory_gib)}${m.local_gb ? ` · ${fmt.n(m.local_gb)} GB ${prov === "aws" ? "local disk" : "disk"}` : ""}`),
      h("div", {}, `${usd(m.usd_hour)}/h (${usd(m.usd_hour * P.hoursAMonth)} a month)${P.spot ? ` · spot ${usd(m.spot_hour)}/h` : ""}`),
      h("div", { class: "small dim" }, [m.processor || P.cpuLabel(m), m.network, `${m.max_pods} pods a node`].filter(Boolean).join(" · ")));
  }

  function drawMachines() {
    if (!have()) { clear(machines); return; }
    if (catErr) { clear(machines, card({ title: "Machines", body: h("div", { class: "error" }, `${P.name}: ${catErr.message}`) })); return; }
    if (!cat) { clear(machines, card({ title: "Machines", body: h("div", { class: "small dim" }, loadingCat ? "Fetching the machines of " + region + "…" : "") })); return; }
    const list = shown();
    const rec = new Set(cat.recommended || []);
    const q = h("input", { class: "input sm cloud-find", placeholder: "Find a type or processor", value: filt.q, oninput: debounce(() => { filt.q = q.value.trim(); drawMachines(); setTimeout(() => { const n = machines.querySelector(".cloud-find"); if (n) { n.focus(); n.setSelectionRange(n.value.length, n.value.length); } }, 0); }, 250) });
    const H = P.hoursAMonth;
    const tbl = table([
      { key: "type", label: "Type", render: (m) => h("span", { class: "row nw" }, h("span", { class: "cloud-sw", style: { background: P.filled(m) ? kindColor(m.kind) : "transparent", borderColor: kindColor(m.kind) } }), h("b", { class: "mono" }, m.type), rec.has(m.type) ? pill("ok", "suggested") : null) },
      { key: "kind", label: "For", render: (m) => kindLabel(m.kind) },
      { key: "cpu", label: "CPU", render: (m) => P.cpuLabel(m), sort: (m) => m.processor || m.arch },
      { key: "vcpu", label: "vCPUs", num: true },
      { key: "memory_gib", label: "Memory", num: true, render: (m) => gib(m.memory_gib) },
      { key: "price", label: filt.spot ? "Spot /h" : P.spot ? "On demand /h" : "Price /h", num: true, sort: price, render: (m) => usd(price(m)) },
      { key: "month", label: "A month", num: true, sort: price, render: (m) => usd(price(m) * H) },
      { key: "per", label: "$ / GiB-month", num: true, sort: (m) => price(m) * H / m.memory_gib, render: (m) => usd(price(m) * H / m.memory_gib, 2) },
      { key: "local_gb", label: prov === "aws" ? "Local disk" : "Disk", num: true, render: (m) => (m.local_gb ? fmt.n(m.local_gb) + " GB" : "–") },
      { key: "max_pods", label: "Pods", num: true, title: "The pods a node of this type may run" },
    ], { rows: list, sortKey: "per", desc: false, compact: true, maxHeight: "280px", onRow: (m) => select(m.type), rowClass: (m) => (m.type === form.machine ? "sel" : "") });
    const ks = kinds();
    clear(machines, card({
      title: "Machines", sub: `${fmt.n(list.length)} of ${fmt.n(cat.machines.length)} in ${cat.region}`, cls: "cloud-machines",
      tools: [
        seg([["all", "All"], ...ks.map(([k, l]) => [k, l])], filt.kind, (v) => { filt.kind = v; drawMachines(); }),
        seg(P.cpus, filt.cpu, (v) => { filt.cpu = v; drawMachines(); }),
        P.spot ? seg([[false, "On demand"], [true, "Spot"]], filt.spot, (v) => { filt.spot = v; drawMachines(); }) : null,
        q,
      ],
      body: h("div", { class: "stack" },
        h("div", { class: "cloud-legend small dim" },
          ks.map(([k, l, c]) => h("span", { class: "row nw" }, h("span", { class: "cloud-sw", style: { background: c, borderColor: c } }), l)),
          h("span", { class: "row nw" }, h("span", { class: "cloud-sw", style: { borderColor: "var(--muted)" } }), P.hollow),
          h("span", { class: "row nw" }, h("span", { class: "cloud-sw ring" }), "ringed: suggested for search (" + P.ringed + ")")),
        chart(list),
        tbl),
    }));
  }

  // ---------------------------------------------------------- the plan
  const body = () => ({
    name: form.name, region, machine: form.machine, nodes: form.nodes, volume_gb: form.volume_gb,
    ...(prov === "aws" ? { spot: form.spot } : { ha: form.ha, version: form.version }),
    cluster: { shards: form.shards, replicas: form.replicas, coordinators: form.coordinators, ...(form.image ? { image: form.image } : {}) },
  });

  const replan = debounce(async () => {
    if (!cat || !form.machine) { drawPlan(); return; }
    const mine = ++seq;
    try { const p = await api(P.base + "/plan", { body: body() }); if (mine !== seq) return; plan = p; planErr = null; }
    catch (e) { if (mine !== seq) return; planErr = e; }
    checked = null;
    drawPlan(); drawWhere();
  }, 200);

  function set(k, v) { form[k] = v; replan(); }

  function nodes() {
    const l = plan.layout, m = plan.machine;
    const box = h("div", { class: "cloud-nodes" });
    for (const n of l.nodes) {
      const used = n.copies.length * l.copy_memory_gib;
      box.append(h("div", { class: "cloud-node" },
        h("div", { class: "row between small" }, h("b", {}, `node ${n.node + 1}`), h("span", { class: "dim mono" }, m.type)),
        h("div", { class: "cloud-copies" }, n.copies.map((c) => h("span", { class: "cloud-copy", style: { "--c": SHARD(c.shard) }, title: `shard ${c.shard}, copy ${c.copy + 1}${c.copy ? "" : " (a primary when the cluster starts)"}` }, `s${c.shard}`, h("small", {}, c.copy ? "r" : "p")))),
        h("div", { class: "barbox", title: `${gib(used)} of ${gib(l.allocatable_gib)} for its copies` }, h("div", { class: "bar", style: { width: clamp(used / Math.max(l.allocatable_gib, 0.1) * 100, 0, 100) + "%" } })),
        h("div", { class: "small dim" }, `${n.copies.length} ${n.copies.length === 1 ? "copy" : "copies"} · ${gib(l.copy_memory_gib)} and ${l.copy_cpu.toFixed(1)} vCPU each`)));
    }
    return box;
  }

  function costBar() {
    const c = plan.cost, lines = c.lines.filter((x) => x.hour != null && x.hour > 0);
    const total = c.hour || 1;
    const colors = ["var(--s1)", "var(--s7)", "var(--s3)", "var(--s4)"];
    const other = form.spot ? c.nodes.on_demand_hour : c.nodes.spot_hour;
    const nodesNow = form.spot ? c.nodes.spot_hour : c.nodes.on_demand_hour;
    return h("div", { class: "stack" },
      h("div", { class: "cloud-totals" },
        h("div", {}, h("b", { class: "num" }, usd(c.hour, 2)), h("span", { class: "small dim" }, " an hour")),
        h("div", {}, h("b", { class: "num" }, usd(c.hour * 24, 2)), h("span", { class: "small dim" }, " a day")),
        h("div", {}, h("b", { class: "num" }, usd(c.month, 0)), h("span", { class: "small dim" }, " a month"))),
      h("div", { class: "cloud-stack" }, lines.map((x, i) => h("span", { style: { width: (x.hour / total * 100) + "%", background: colors[i % colors.length] }, title: `${x.what}: ${usd(x.hour, 3)}/h` }))),
      h("div", { class: "stack", style: { gap: "4px" } }, c.lines.map((x, i) => h("div", { class: "cloud-line small" },
        h("span", {}, h("span", { class: "cloud-sw", style: { background: colors[i % colors.length], borderColor: colors[i % colors.length] } }), x.what),
        h("span", { class: "num" }, x.hour == null ? "price unknown" : `${usd(x.hour, 3)}/h · ${usd(x.month, 2)}/month`)))),
      P.spot && other != null && nodesNow != null ? h("div", { class: "small dim" }, form.spot
        ? `On demand, the nodes would cost ${usd(other, 3)}/h, ${fmt.pct(1 - nodesNow / other, 0)} more than spot.`
        : `On spot, the nodes would cost ${usd(other, 3)}/h (${fmt.pct(1 - other / nodesNow, 0)} less), and may be taken back with two minutes' notice.`) : null,
      h("div", { class: "small dim" }, `Not counted: ${c.not_counted}.`));
  }

  async function check() {
    checking = true; drawPlan();
    try { checked = (await api(P.base + "/validate", { body: body() })).validated; } catch (e) { checked = { ok: false, error: e.message }; }
    checking = false; drawPlan();
  }

  function checkedLine() {
    if (!checked) return null;
    if (prov === "aws") return checked.ok
      ? h("div", { class: "row small" }, pill("ok", "CloudFormation accepts the template"), checked.answer?.capabilities?.length ? `It needs ${checked.answer.capabilities.join(", ")}: it makes IAM roles.` : "")
      : h("div", { class: "row small" }, pill("bad", "CloudFormation refused the template"), checked.error);
    if (checked.error) return h("div", { class: "row small" }, pill("bad", "DigitalOcean did not answer"), checked.error);
    return h("div", { class: "stack", style: { gap: "4px" } },
      h("div", { class: "row small" }, checked.ok ? pill("ok", "DigitalOcean would take this cluster") : pill("bad", "DigitalOcean would not take this cluster")),
      (checked.checks || []).map((c) => h("div", { class: "row nw small" }, icon(c.ok ? "okc" : "bad"), h("span", {}, c.check))));
  }

  function drawPlan() {
    if (!have() || !cat) { clear(planEl); return; }
    const name = h("input", { class: "input sm mono", value: form.name, size: 14, onchange: () => set("name", name.value.trim()) });
    const image = h("input", { class: "input sm mono", value: form.image, placeholder: "ghcr.io/chloetech-ai/strata-server:latest", size: 34, onchange: () => set("image", image.value.trim()) });
    const version = prov === "digitalocean" ? h("select", { class: "select sm", onchange: () => set("version", version.value) },
      h("option", { value: "", selected: !form.version }, `newest (${cat.versions?.[0] || "–"})`), (cat.versions || []).map((v) => h("option", { value: v, selected: v === form.version }, v))) : null;
    const controls = h("div", { class: "cloud-controls" },
      h("label", { class: "cloud-step" }, h("span", { class: "small dim" }, "Name"), h("div", { class: "row nw" }, h("span", { class: "small dim mono" }, "strata-"), name)),
      h("div", { class: "cloud-step" }, h("span", { class: "small dim" }, "Machine"), h("b", { class: "mono" }, form.machine || "pick one above")),
      stepper(form.nodes, 1, 100, (v) => set("nodes", v), "Nodes"),
      stepper(form.shards, 1, 64, (v) => set("shards", v), "Shards"),
      stepper(form.replicas, 1, 9, (v) => set("replicas", v), "Copies a shard"),
      stepper(form.coordinators, 1, 9, (v) => set("coordinators", v), "Coordinators"),
      stepper(form.volume_gb, 1, 16384, (v) => set("volume_gb", v), "Volume a copy", " GiB"),
      prov === "aws"
        ? h("div", { class: "cloud-step" }, h("span", { class: "small dim" }, "Nodes bought"), toggle(form.spot, (v) => set("spot", v), form.spot ? "spot" : "on demand"))
        : [h("div", { class: "cloud-step" }, h("span", { class: "small dim" }, "Control plane"), toggle(form.ha, (v) => set("ha", v), form.ha ? "highly available" : "standard (free)")),
          h("label", { class: "cloud-step" }, h("span", { class: "small dim" }, "Kubernetes"), version)],
      h("label", { class: "cloud-step" }, h("span", { class: "small dim" }, "Image"), image));
    if (planErr) { clear(planEl, card({ title: "Plan", body: h("div", { class: "stack" }, controls, h("div", { class: "error" }, planErr.message)) })); return; }
    if (!plan) { clear(planEl, card({ title: "Plan", body: h("div", { class: "stack" }, controls, h("div", { class: "small dim" }, "Planning…")) })); return; }
    const stack = plan.stack;
    drawCreate();
    const files = [
      prov === "aws"
        ? [stack + ".json", "the CloudFormation template", () => download(stack + ".json", JSON.stringify(plan.template, null, 2))]
        : [stack + "-doks.json", "DOKS's request (POST /v2/kubernetes/clusters)", () => download(stack + "-doks.json", JSON.stringify(plan.request, null, 2))],
      [stack + "-operator.yaml", "the operator", () => download(stack + "-operator.yaml", plan.manifests.operator, "text/yaml")],
      [stack + "-cluster.yaml", prov === "aws" ? "the namespace, a gp3 storage class and the StrataCluster" : "the namespace and the StrataCluster", () => download(stack + "-cluster.yaml", plan.manifests.cluster, "text/yaml")],
    ];
    const q = plan.quota;
    const quotaLine = prov === "aws"
      ? h("div", { class: "row small" }, q.vcpus != null && q.needs > q.vcpus ? pill("bad", "over quota") : pill("ok", "within quota"), `${q.needs} vCPUs of ${q.kind} instances; the account may run ${q.vcpus == null ? "an unknown number" : fmt.n(q.vcpus)} in ${plan.region}.`)
      : h("div", { class: "row small" }, q.droplets != null && q.running != null && q.running + q.needs > q.droplets ? pill("bad", "over the droplet limit") : pill("ok", "within the droplet limit"), `${q.needs} droplets; the account may run ${q.droplets == null ? "an unknown number" : fmt.n(q.droplets)} and runs ${q.running == null ? "an unknown number" : fmt.n(q.running)}.`);
    clear(planEl, card({
      title: "Plan", sub: `${plan.stack} in ${plan.region}${plan.version ? ", Kubernetes " + plan.version : ""}`,
      tools: [h("button", { class: "btn sm", disabled: checking, onclick: check, title: P.checkTitle }, icon("check"), checking ? "Checking…" : P.check)],
      body: h("div", { class: "stack" },
        controls,
        h("div", { class: "cloud-plan" },
          h("div", { class: "stack" },
            h("h4", {}, "Where the copies go"),
            nodes(),
            h("div", { class: "small dim" }, `${plan.layout.pods_per_node} pods on the busiest node, of the ${plan.layout.max_pods} a ${plan.machine.type} may run. Each node also runs the cloud's own pods; the coordinators and the operator go where there is room.`)),
          h("div", { class: "stack" },
            h("h4", {}, "What it costs"),
            costBar(),
            quotaLine)),
        plan.warnings.length ? h("div", { class: "stack cloud-warn" }, plan.warnings.map((w) => h("div", { class: "row nw small" }, icon("warn"), h("span", {}, w)))) : null,
        checkedLine(),
        h("h4", {}, "Make it"),
        createEl,
        h("div", { class: "small dim" }, "Or make it yourself, with these files:"),
        h("div", { class: "row" }, files.map(([f, what, go]) => h("button", { class: "btn sm", title: "Download " + what, onclick: go }, icon("download"), f))),
        h("div", { class: "small dim" }, prov === "aws" ? "With the AWS CLI and kubectl, from the folder the files are in." : "With doctl (signed in with a token that may create clusters) and kubectl, from the folder the files are in; doctl points kubectl at the new cluster.", " The nodes pull the image above, so it must be one they can reach."),
        cmds(plan.commands),
        h("details", {}, h("summary", { class: "small" }, "Delete it"),
          h("div", { class: "small dim", style: { margin: "6px 0" } }, prov === "aws" ? "The copies' volumes belong to the EBS driver, not to the stack: delete their claims first, or they stay and cost." : "The copies' volumes belong to the CSI driver: delete their claims first (or the cluster with --dangerous), or they stay and cost."),
          cmds(plan.teardown))),
    }));
  }

  // ---------------------------------------------------------- making it
  /** Why the account would not start the nodes, if it would not. */
  function blocked() {
    const q = plan.quota;
    const same = clusters.find((c) => c.provider === prov && c.cluster === plan.cluster_name && c.phase !== "deleted" && !(c.phase === "failed" && !c.cloud_id));
    if (same) return `${plan.cluster_name} was made from here already (${PHASES[same.phase][1]}): delete it first, or change its name.`;
    if (prov === "aws") return q.vcpus != null && q.needs > q.vcpus ? `The account may run ${fmt.n(q.vcpus)} vCPUs of ${q.kind} instances in ${plan.region} and the nodes need ${q.needs}: ask AWS for more first.` : null;
    return q.droplets != null && q.running != null && q.running + q.needs > q.droplets ? `The account runs ${q.running} of its ${q.droplets} droplets and the nodes need ${q.needs}: ask DigitalOcean for more first.` : null;
  }

  /** The Create button, redrawn alone (the plan's inputs keep their focus). */
  function drawCreate() {
    if (!plan) return;
    const why = blocked();
    clear(createEl, h("div", { class: "row" },
      h("button", { class: "btn danger solid", disabled: creating || !!why, onclick: create, title: why || `Make ${plan.stack} on ${P.name} now` }, icon("cloud"), creating ? "Asking…" : `Create it on ${P.name}…`),
      h("span", { class: "small dim" }, why || `About ${usd(plan.cost.hour, 2)} an hour from then until it is deleted, billed to the account whose key is saved here. This page follows it and deletes it.`)));
  }

  async function create() {
    const name = plan.cluster_name, p = plan;
    const ok = await typed({
      title: `Create ${name} on ${P.name}?`, name, ok: "Create it",
      body: h("div", { class: "stack" },
        h("p", { class: "dim", style: { margin: 0 } }, `${p.layout.nodes.length} × ${p.machine.type} in ${p.region}, on ${P.k8s}: about ${usd(p.cost.hour, 2)} an hour, ${usd(p.cost.month, 0)} a month, from now until it is deleted.`),
        h("p", { class: "dim", style: { margin: 0 } }, prov === "aws"
          ? "CloudFormation makes the network, the EKS cluster and its nodes (15 to 20 minutes); then this server installs Strata's operator and the cluster, and follows its copies until they answer."
          : "DigitalOcean makes the cluster and its nodes (5 to 10 minutes); then this server installs Strata's operator and the cluster, and follows its copies until they answer."),
        p.warnings.length ? h("div", { class: "stack cloud-warn" }, p.warnings.map((w) => h("div", { class: "row nw small" }, icon("warn"), h("span", {}, w)))) : null),
    });
    if (!ok) return;
    creating = true; drawCreate();
    try {
      const m = await api("/v1/admin/cloud/clusters", { body: { provider: prov, plan: body(), confirm: name } });
      clusters = [m, ...clusters.filter((c) => c.id !== m.id)];
      toast(`${name} is on its way`, "ok");
      drawMade(); poll(2000);
      made.scrollIntoView({ behavior: "smooth", block: "start" });
    } catch (e) { toast(e.message, "bad", 8000); }
    creating = false; drawCreate();
  }

  async function remove(c) {
    const ok = await typed({
      title: `Delete ${c.cluster}?`, name: c.cluster, ok: "Delete it",
      body: h("p", { class: "dim", style: { margin: 0 } }, `This deletes the StrataCluster and its volumes (its data), then ${c.provider === "aws" ? "the stack: the nodes, the EKS cluster and the network" : "the DOKS cluster, its nodes and what it made"}. It cannot be undone.`),
    });
    if (!ok) return;
    try { const m = await api(`/v1/admin/cloud/clusters/${encodeURIComponent(c.id)}/delete`, { body: { confirm: c.cluster } }); clusters = clusters.map((x) => (x.id === m.id ? m : x)); drawMade(); poll(2000); }
    catch (e) { toast(e.message, "bad", 8000); }
  }

  async function forget(c) {
    try { await api(`/v1/admin/cloud/clusters/${encodeURIComponent(c.id)}`, { method: "DELETE" }); clusters = clusters.filter((x) => x.id !== c.id); drawMade(); }
    catch (e) { toast(e.message, "bad"); }
  }

  async function loadMade() {
    try { clusters = (await api("/v1/admin/cloud/clusters")).clusters || []; } catch { return; }
    if (!alive) return;
    drawMade(); poll(); drawCreate();
  }

  /** Follows the clusters: every few seconds while one is on its way, every half minute while one is up. */
  function poll(ms) {
    clearTimeout(pollT);
    if (!alive || !clusters.length) return;
    const busy = clusters.some((c) => ON_ITS_WAY.has(c.phase));
    if (!busy && !clusters.some((c) => c.phase === "ready")) return;
    pollT = setTimeout(loadMade, ms ?? (busy ? 3000 : 30000));
  }

  function drawMade() {
    if (!clusters.length) { clear(made); return; }
    const up = clusters.filter((c) => c.phase !== "deleted" && c.phase !== "failed").length;
    const billing = clusters.filter((c) => c.phase !== "deleted").reduce((a, c) => a + c.cost_hour, 0);
    clear(made, card({
      title: "Clusters made here", cls: "cloud-made",
      sub: up ? `${up} up or on the way, about ${usd(billing, 2)} an hour` : "none running",
      tools: [h("button", { class: "btn sm ghost", title: "Ask again now", onclick: loadMade }, icon("refresh"))],
      body: h("div", { class: "stack" }, clusters.map(madeRow)),
    }));
  }

  function madeRow(c) {
    const order = ["creating", "installing", "starting", "ready"];
    const at = order.indexOf(c.phase);
    const stages = [[c.provider === "aws" ? "Stack" : "Cluster", "the cloud makes the cluster and its nodes"], ["Strata", "the operator and the cluster are applied"], ["Copies", "the operator starts every copy"], ["Ready", "every copy answers"]];
    const track = at < 0 ? null : h("div", { class: "cloud-track" }, stages.map(([label, what], i) =>
      h("span", { class: "cloud-stage" + (i < at || c.phase === "ready" ? " done" : i === at ? " now" : ""), title: what }, label)));
    const last = c.steps[c.steps.length - 1];
    const progress = c.progress && c.phase === "creating" ? ` (${c.progress.done} of ${c.progress.of} ${c.provider === "aws" ? "resources" : "nodes"})` : "";
    const action = c.phase === "deleted" || (c.phase === "failed" && !c.cloud_id)
      ? h("button", { class: "btn sm", title: "Take it off this list", onclick: () => forget(c) }, icon("x"), "Forget")
      : c.phase === "deleting" ? null
      : h("button", { class: "btn sm danger", onclick: () => remove(c) }, icon("trash"), "Delete");
    return h("div", { class: "cloud-made-row" },
      h("div", { class: "row between" },
        h("div", { class: "row nw" }, h("b", { class: "mono" }, c.cluster), pill(...PHASES[c.phase]),
          h("span", { class: "small dim" }, `${c.provider === "aws" ? "AWS" : "DigitalOcean"} · ${c.region} · ${c.nodes} × ${c.machine}`)),
        h("div", { class: "row nw" },
          h("span", { class: "small dim num", title: "at the planned price, from when it was asked for" }, `${usd(c.cost_so_far, 2)} so far${c.phase === "deleted" ? "" : ` · ${usd(c.cost_hour, 2)}/h`}`),
          action)),
      track,
      last ? h("div", { class: "small" }, h("span", { class: "dim" }, fmt.ago(last.at_ms) + " · "), last.text + progress) : null,
      c.summary && !["deleted", "ready"].includes(c.phase) && !(last && last.text.includes(c.summary)) ? h("div", { class: "small" }, h("span", { class: "dim" }, "The StrataCluster: "), c.summary) : null,
      c.phase === "failed" && c.error ? h("div", { class: "error small" }, c.error, c.cloud_id ? " Delete it: what the cloud made may still bill." : "") : null,
      c.connect.length ? h("details", {}, h("summary", { class: "small" }, "Open its command center"),
        h("div", { class: "small dim", style: { margin: "6px 0" } }, "Point kubectl at it, forward the coordinator's port, then open http://127.0.0.1:7700."),
        cmds(c.connect)) : null,
      h("details", {}, h("summary", { class: "small" }, `What happened (${c.steps.length})`),
        h("div", { class: "stack small cloud-steps" }, c.steps.map((st) => h("div", { class: "row nw" }, h("span", { class: "dim num" }, fmt.time(st.at_ms)), h("span", {}, st.text))))));
  }

  load();
  loadMade();
  return () => { alive = false; clearTimeout(pollT); hideTip(); switchProvider = null; };
}
