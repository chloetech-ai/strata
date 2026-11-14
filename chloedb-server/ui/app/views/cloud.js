// Cloud: a ChloeDB cluster on Amazon EKS or DigitalOcean Kubernetes (DOKS),
// planned from this page. The account's key is kept on this server (never
// shown again, every change audited); the region's machines are drawn by
// price and memory with their live prices and the account's limit; the
// plan lays the shards' copies on the nodes, prices them line by line,
// says what would stop them, and hands over what makes the cluster (a
// CloudFormation template, or DOKS's request) and the Kubernetes
// manifests, checked by the cloud itself. **Create it** makes the cluster
// from here, after its name is typed again: this server asks the cloud,
// installs ChloeDB's operator through the new cluster's API, follows its
// copies until they answer, and deletes it (its volumes first) when asked.
// A ChloeDB cluster the account holds that this server did not make (made
// from another server, or forgotten here) is found on asking and deleted
// the same way, its name typed again; not the one this server runs on.

import * as core from "../core.js";
import { h, s, icon, clear, fmt, card, api, toast, confirm, seg, toggle, pill, empty, table, tipOn, hideTip, debounce, copy, failure, clamp, modal, menu, connectionName, experimental } from "../core.js";
import { state } from "../data.js";

export default {
  id: "cloud", title: "Cloud setup", icon: "cloud", group: "Change",
  keywords: "cloud aws amazon eks digitalocean doks droplet kubernetes k8s credentials access key token region instance instances machine machines size sizes ec2 graviton price prices cost spot quota limit cloudformation template deploy",
  render, onParams,
};

let switchProvider = null;
// Keys from the server's environment are used only once someone asks, in
// this tab: opening the page calls no cloud (CD-1).
const envAsked = {};
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
    check: "Check with AWS", checkTitle: "CloudFormation's own check of the template: free, and makes nothing",
    asks: [["the regions", "ec2:DescribeRegions"], ["the machines of a region", "ec2:DescribeInstanceTypes"], ["their prices", "pricing:GetProducts, ec2:DescribeSpotPriceHistory"], ["the account's vCPU quota", "servicequotas:GetServiceQuota"]],
    trail: "Each call is in the account's CloudTrail, made by the identity of those keys.",
  },
  digitalocean: {
    name: "DigitalOcean", k8s: "DigitalOcean Kubernetes (DOKS)", base: "/v1/admin/digitalocean", keyPath: "/token", status: (st) => st?.token,
    regions: ["nyc3", "nyc1", "sfo3", "sfo2", "ams3", "lon1", "fra1", "tor1", "sgp1", "blr1", "syd1", "atl1"],
    region: "nyc3", spot: false, cpus: [["any", "Any CPU"], ["regular", "Regular"], ["intel", "Premium Intel"], ["amd", "Premium AMD"]],
    cpu: (m) => (m.family.endsWith("-intel") ? "intel" : m.family.endsWith("-amd") ? "amd" : "regular"), filled: (m) => !m.burstable, cpuLabel: (m) => (m.family.endsWith("-intel") ? "Premium Intel" : m.family.endsWith("-amd") ? "Premium AMD" : m.burstable ? "Shared" : "Dedicated"),
    hollow: "hollow: shared CPUs", ringed: "dedicated general and memory-optimized sizes of 2 to 8 vCPUs", hoursAMonth: 672,
    check: "Check with DigitalOcean", checkTitle: "DigitalOcean's own answer, fetched now: the account, the region, the size, the version and the droplet limit. Makes nothing",
    asks: [["the regions", "GET /v2/regions"], ["the sizes and their prices", "GET /v2/sizes, /v2/kubernetes/options"], ["the account and its droplet limit", "GET /v2/account"]],
    trail: "Each call is made with that token, and counts against its rate limit.",
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
  creating: ["pending", "creating"], installing: ["pending", "installing ChloeDB"], starting: ["pending", "starting its copies"],
  ready: ["ok", "ready"], deleting: ["warn", "deleting"], deleted: ["disabled", "deleted"], failed: ["bad", "failed"],
};
const ON_ITS_WAY = new Set(["creating", "installing", "starting", "deleting"]);
/** The plan's machine for the cheapest that gives each copy the floor (crate::cloud::SMALLEST). */
const SMALLEST = "smallest";
/** The demo products a cluster may start with, split over its shards. */
const DEMO_ROWS = 10000;
/** Delete it after: hours, or kept until deleted. */
const AFTER = [[null, "keep it"], [4, "4 hours"], [8, "8 hours"], [24, "a day"], [72, "3 days"], [168, "a week"]];
/** A time this week, to the minute: "Thu 14:05". */
const when = (ms) => new Date(ms).toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit", hour12: false });
const hoursLabel = (h) => (AFTER.find(([v]) => v === h)?.[1] || `${h} hours`);

/** How far a press of − or + goes: further as the number grows. */
const stride = (v) => (v >= 1000 ? 100 : v >= 200 ? 50 : v >= 50 ? 10 : v >= 20 ? 5 : 1);
/** A number with − and +, which may also be typed (any value from min to max). */
/** A Kubernetes quantity ("500Gi", "1Ti", "200G") in GiB, or null. */
function quantityGib(q) {
  const m = /^\s*([\d.]+)\s*(Ki|Mi|Gi|Ti|Pi|K|M|G|T|P)?\s*$/.exec(String(q ?? ""));
  if (!m) return null;
  const n = +m[1], u = m[2] || "";
  const bytes = n * ({ Ki: 2 ** 10, Mi: 2 ** 20, Gi: 2 ** 30, Ti: 2 ** 40, Pi: 2 ** 50, K: 1e3, M: 1e6, G: 1e9, T: 1e12, P: 1e15 }[u] || 1);
  const g = Math.round(bytes / 2 ** 30);
  return g >= 1 ? g : null;
}
function stepper(value, min, max, onChange, label, unit = "") {
  const inp = h("input", { class: "input sm num ro-cnum", type: "number", inputmode: "numeric", min, max, value: String(value), "aria-label": label });
  const less = h("button", { class: "btn xs", type: "button", "aria-label": "fewer " + label, title: `${stride(value - 1)} fewer` }, icon("minus"));
  const more = h("button", { class: "btn xs", type: "button", "aria-label": "more " + label, title: `${stride(value)} more` }, icon("plus"));
  const sync = () => { inp.value = String(value); less.disabled = value <= min; more.disabled = value >= max; less.title = `${stride(value - 1)} fewer`; more.title = `${stride(value)} more`; };
  const set = (x) => { x = clamp(Math.round(x) || min, min, max); if (x !== value) { value = x; onChange(x); } sync(); };
  less.onclick = () => { const d = stride(value - 1); set(value % d ? value - (value % d) : value - d); };
  more.onclick = () => { const d = stride(value); set(value - (value % d) + d); };
  inp.onchange = () => set(+inp.value);
  inp.onkeydown = (e) => { if (e.key === "Enter") { e.preventDefault(); set(+inp.value); } };
  sync();
  return h("div", { class: "cloud-step" },
    h("span", { class: "small dim" }, label, unit ? ` (${unit.trim()})` : ""),
    h("div", { class: "row nw" }, less, inp, more));
}

const remembered = () => { try { return localStorage.getItem("chloedb.cloud") || "aws"; } catch { return "aws"; } };

function render(page, r) {
  let prov = PROVIDERS[r?.params?.get("provider")] ? r.params.get("provider") : remembered();
  let P = PROVIDERS[prov];
  const head = h("div"), made = h("div"), foundEl = h("div"), creds = h("div"), where = h("div"), machines = h("div"), planEl = h("div");
  page.append(head, made, foundEl, h("div", { class: "cloud-top" }, creds, where), machines, planEl);
  let clusters = [], pollT = null, alive = true, creating = false;
  const createEl = h("div");

  let status, regions, region, cat, catErr, loadingCat, editing, filt, form, plan, planErr, checked, checking;
  let seq = 0;
  let running = null;   // this cluster's size: {shards, replicas, coordinators}
  let here = null;      // this server's tables, which a new cluster may bring
  let sized = false;    // the size was set by hand (or from this cluster's)
  // The account's ChloeDB clusters this server does not follow: null until
  // someone looks (opening the page asks no cloud), with the region looked in.
  let found = null, foundErr = null, looking = false, foundRegion = null;
  function reset() {
    found = null; foundErr = null; looking = false; foundRegion = null;
    status = null; regions = null; region = null; cat = null; catErr = null; loadingCat = false; editing = false;
    filt = { kind: "all", cpu: "any", spot: false, q: "" };
    // The machine starts as the cheapest that gives each copy a GiB and
    // half a core (the server's crate::cloud::SMALLEST); the cluster
    // starts with no tables, and is kept until it is deleted.
    form = { name: "search", machine: SMALLEST, nodes: 3, spot: false, ha: false, version: "", volume_gb: 10, shards: 2, replicas: 3, coordinators: 1, image: "", demo: false, delete_after: null, bring: [] };
    plan = null; planErr = null; checked = null; checking = false; seq++;
  }
  reset();

  const key = () => P.status(status);
  const have = () => !!key();
  /** Whether the page may ask the cloud: a stored key, or the environment's once asked for. */
  const may = () => have() && (key().source === "stored" || !!envAsked[prov]);

  switchProvider = (p) => {
    if (!PROVIDERS[p] || p === prov) return;
    prov = p; P = PROVIDERS[p];
    try { localStorage.setItem("chloedb.cloud", p); } catch { /* private mode */ }
    reset(); clear(creds); clear(where); clear(machines); clear(planEl); load();
  };

  async function load() {
    const mine = seq;
    drawHead();
    try { status = await api(P.base); } catch (e) { if (mine !== seq) return; clear(creds, failure(e, "The cloud view")); return; }
    if (mine !== seq) return;
    drawHead(); drawCreds(); drawFound();
    if (may()) {
      region = region || key().region || P.region;
      if (!regions) api(P.base + "/regions").then((x) => { if (mine === seq) { regions = x.regions; drawWhere(); } }).catch(() => { regions = null; });
      await loadCatalog(false);
    } else { cat = null; drawWhere(); drawMachines(); drawPlan(); }
  }

  /** This cluster's size, from the operator's spec or the cluster's shards; asks no cloud. */
  async function loadRunning() {
    let k = null;
    try { k = await api("/v1/kube"); } catch { /* not on Kubernetes */ }
    const sp = k?.managed ? k.spec || {} : null;
    const c = state.cluster;
    const shards = sp?.shards ?? c?.shards?.length;
    const reps = sp?.replicas ?? (c?.shards?.length ? Math.max(...c.shards.map((x) => (x.replicas || []).length)) : null);
    const coords = sp?.coordinators ?? (c?.coordinators?.length || null);
    if (!shards || !reps) return;
    // The volume a copy has (NEW-cluster-admin-8): the spec's size, else the
    // largest disk a copy reports; the plan's cost starts from it.
    const disks = (c?.shards || []).flatMap((x) => x.replicas || []).map((r) => r.detail?.load?.disk_total_bytes).filter((b) => b > 0);
    const volume = quantityGib(sp?.storage?.size) ?? (disks.length ? Math.round(Math.max(...disks) / 2 ** 30) : null);
    running = { shards, replicas: reps, coordinators: coords || 1, name: k?.name || null, volume_gb: volume, volumeFrom: sp?.storage?.size ? `spec.storage.size ${sp.storage.size}` : volume ? "the largest disk a copy reports" : null };
    if (!sized) useRunning(false);
    else drawPlan();
  }
  /** This server's tables: what a new cluster may bring once it is ready. */
  async function loadHere() {
    try { here = ((await api("/v1/tables")).tables || []).map((t) => t.table); } catch { here = []; }
    if (alive && cat) drawPlan();
  }
  /** Starts the plan from this cluster's size: its shards and copies, a node for about every 4 copies. */
  function useRunning(byHand = true) {
    if (!running) return;
    form.shards = running.shards; form.replicas = running.replicas; form.coordinators = running.coordinators;
    if (running.volume_gb) form.volume_gb = clamp(running.volume_gb, 1, 16384);
    form.nodes = clamp(Math.max(form.replicas, Math.ceil(running.shards * running.replicas / 4)), 1, 100);
    if (byHand) sized = true;
    if (cat) replan(); else drawPlan();
  }

  async function loadCatalog(fresh) {
    const mine = seq;
    loadingCat = true; catErr = null; drawWhere(); drawMachines();
    try {
      const c = await api(`${P.base}/machines?region=${encodeURIComponent(region)}${fresh ? "&fresh=1" : ""}`);
      if (mine !== seq) return;
      cat = c;
      if (!form.machine || (form.machine !== SMALLEST && !cat.machines.some((m) => m.type === form.machine))) form.machine = SMALLEST;
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
      h("div", {}, h("h1", {}, "Cloud setup", " ", experimental("Cloud setup")), h("p", {}, `Plan a new ChloeDB cluster on ${P.k8s}: machines with their prices, where the copies go, what it costs, and what makes it.`)),
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
    if (!(core.isNarrow?.() || core.isPhone?.())) setTimeout(() => first.focus(), 0); // a phone would open its keyboard over the page
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
    if (have() && !may()) {
      const k = key();
      clear(where, card({
        title: "Region and limits", sub: "nothing asked yet",
        body: h("div", { class: "stack" },
          h("div", {}, `This server has ${prov === "aws" ? "AWS keys" : "a DigitalOcean token"} in its environment (`, h("span", { class: "mono" }, prov === "aws" ? k.access_key_id : k.token), prov === "aws" && k.region ? `, ${k.region}` : "", "). Opening this page asked ", P.name, " nothing."),
          h("div", { class: "small dim" }, `Using ${prov === "aws" ? "them" : "it"} asks ${P.name} for:`),
          h("ul", { class: "ro-casks small" }, P.asks.map(([what, call]) => h("li", {}, what, " ", h("span", { class: "mono dim" }, `(${call})`)))),
          h("div", { class: "small dim" }, P.trail, " Nothing is made: making a cluster asks again, with its name typed."),
          h("div", { class: "row" },
            h("button", { class: "btn primary", onclick: () => { envAsked[prov] = true; load(); } }, `Use the environment's ${prov === "aws" ? "keys" : "token"}`),
            h("button", { class: "btn", onclick: () => { editing = true; drawCreds(); } }, icon("plus"), prov === "aws" ? "Save a key here" : "Save a token here"))),
      }));
      return;
    }
    if (!have()) { clear(where, card({ title: "Region and limits", body: empty("cloud", `No ${P.name} key yet`, prov === "aws" ? "Save an access key to see the machines of a region, their prices and your account's quota." : "Save a token to see the sizes of a region, their prices and your account's droplet limit.") })); return; }
    const list = (regions || P.regions).map((x) => (typeof x === "string" ? { slug: x, name: x } : x));
    const sel = h("select", { class: "select", onchange: () => { region = sel.value; plan = null; checked = null; drawFound(); loadCatalog(false); } }, list.map((x) => h("option", { value: x.slug, selected: x.slug === region }, x.name === x.slug ? x.slug : `${x.name} (${x.slug})`)));
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
  /** The machine the plan runs on: the one picked, or the one the server found for SMALLEST. */
  const chosen = () => (form.machine === SMALLEST ? plan?.machine?.type : form.machine);

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
    const order = list.slice().sort((a, b) => (a.type === chosen()) - (b.type === chosen()));
    for (const m of order) {
      const p = price(m); if (!(p > 0)) continue;
      const on = m.type === chosen();
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
    if (!may()) { clear(machines); return; }
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
    ], { rows: list, sortKey: "per", desc: false, compact: true, maxHeight: "280px", onRow: (m) => select(m.type), rowClass: (m) => (m.type === chosen() ? "sel" : "") });
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
    cluster: { shards: form.shards, replicas: form.replicas, coordinators: form.coordinators, ...(form.image ? { image: form.image } : {}), ...(form.demo ? { demo: DEMO_ROWS } : {}) },
  });

  const replan = debounce(async () => {
    if (!cat || !form.machine) { drawPlan(); return; }
    const mine = ++seq;
    try { const p = await api(P.base + "/plan", { body: body() }); if (mine !== seq) return; plan = p; planErr = null; }
    catch (e) { if (mine !== seq) return; planErr = e; }
    checked = null;
    drawPlan(); drawWhere();
    if (form.machine === SMALLEST) drawMachines();
  }, 200);

  function set(k, v) { form[k] = v; if (["shards", "replicas", "coordinators", "nodes"].includes(k)) sized = true; replan(); }

  function nodes() {
    const l = plan.layout, m = plan.machine;
    const box = h("div", { class: "cloud-nodes" });
    // A big cluster: the first nodes, and how the others look.
    const many = l.nodes.length > 12;
    for (const n of many ? l.nodes.slice(0, 12) : l.nodes) {
      const used = n.copies.length * l.copy_memory_gib;
      box.append(h("div", { class: "cloud-node" },
        h("div", { class: "row between small" }, h("b", {}, `node ${n.node + 1}`), h("span", { class: "dim mono" }, m.type)),
        h("div", { class: "cloud-copies" }, n.copies.map((c) => h("span", { class: "cloud-copy", style: { "--c": SHARD(c.shard) }, title: `shard ${c.shard}, copy ${c.copy + 1}${c.copy ? "" : " (a primary when the cluster starts)"}` }, `s${c.shard}`, h("small", {}, c.copy ? "r" : "p")))),
        h("div", { class: "barbox", title: `${gib(used)} of ${gib(l.allocatable_gib)} for its copies` }, h("div", { class: "bar", style: { width: clamp(used / Math.max(l.allocatable_gib, 0.1) * 100, 0, 100) + "%" } })),
        h("div", { class: "small dim" }, `${n.copies.length} ${n.copies.length === 1 ? "copy" : "copies"} · ${gib(l.copy_memory_gib)} and ${l.copy_cpu.toFixed(1)} vCPU each`)));
    }
    if (many) {
      const rest = l.nodes.slice(12), counts = rest.map((n) => n.copies.length);
      box.append(h("div", { class: "cloud-node ro-cmore" }, h("b", {}, `${fmt.n(rest.length)} more nodes`),
        h("div", { class: "small dim" }, `${Math.min(...counts) === Math.max(...counts) ? counts[0] : `${Math.min(...counts)} to ${Math.max(...counts)}`} copies each, laid out the same way: shard by shard, a shard's copies on different nodes.`)));
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
    if (!may() || !cat) { clear(planEl); return; }
    const name = h("input", { class: "input sm mono", value: form.name, size: 14, onchange: () => set("name", name.value.trim()) });
    const image = h("input", { class: "input sm mono", value: form.image, placeholder: "ghcr.io/chloetech-ai/chloedb-server:latest", size: 34, onchange: () => set("image", image.value.trim()) });
    const afterSel = h("select", { class: "select sm", title: "This server deletes it then (and its data), or as soon as it runs again if it was off: nothing in the cloud does", onchange: () => { form.delete_after = afterSel.value ? Number(afterSel.value) : null; drawCreate(); } },
      AFTER.map(([v, l]) => h("option", { value: v ?? "", selected: (v ?? null) === form.delete_after }, l)));
    const version = prov === "digitalocean" ? h("select", { class: "select sm", onchange: () => set("version", version.value) },
      h("option", { value: "", selected: !form.version }, `newest (${cat.versions?.[0] || "–"})`), (cat.versions || []).map((v) => h("option", { value: v, selected: v === form.version }, v))) : null;
    const controls = h("div", { class: "cloud-controls" },
      h("label", { class: "cloud-step" }, h("span", { class: "small dim" }, "Name"), h("div", { class: "row nw" }, h("span", { class: "small dim mono" }, "strata-"), name)),
      h("div", { class: "cloud-step" }, h("span", { class: "small dim" }, "Machine"), form.machine === SMALLEST
        ? h("div", { class: "row nw", title: "The cheapest machine whose nodes give each copy at least 1 GiB and half a core, with room for its pods. Pick one above to choose it yourself." }, h("b", { class: "mono" }, plan?.machine?.type || "…"), pill("ok", "smallest that fits"))
        : h("div", { class: "row nw" }, h("b", { class: "mono" }, form.machine || "pick one above"), h("button", { class: "btn sm ghost", title: "The cheapest machine whose nodes give each copy at least 1 GiB and half a core", onclick: () => select(SMALLEST) }, "Smallest that fits"))),
      stepper(form.nodes, 1, 100, (v) => set("nodes", v), "Nodes"),
      stepper(form.shards, 1, 4096, (v) => set("shards", v), "Shards"),
      stepper(form.replicas, 1, 9, (v) => set("replicas", v), "Copies a shard"),
      stepper(form.coordinators, 1, 16, (v) => set("coordinators", v), "Coordinators"),
      stepper(form.volume_gb, 1, 16384, (v) => set("volume_gb", v), "Volume a copy", " GiB"),
      prov === "aws"
        ? h("div", { class: "cloud-step" }, h("span", { class: "small dim" }, "Nodes bought"), toggle(form.spot, (v) => set("spot", v), form.spot ? "spot" : "on demand"))
        : [h("div", { class: "cloud-step" }, h("span", { class: "small dim" }, "Control plane"), toggle(form.ha, (v) => set("ha", v), form.ha ? "highly available" : "standard (free)")),
          h("label", { class: "cloud-step" }, h("span", { class: "small dim" }, "Kubernetes"), version)],
      h("label", { class: "cloud-step" }, h("span", { class: "small dim" }, "Image"), image),
      h("div", { class: "cloud-step" }, h("span", { class: "small dim" }, "Starts with"), toggle(form.demo, (v) => set("demo", v), form.demo ? `${fmt.n(DEMO_ROWS)} demo products` : "no tables")),
      h("label", { class: "cloud-step" }, h("span", { class: "small dim" }, "Delete it after"), afterSel),
      here?.length ? h("div", { class: "cloud-step" }, h("span", { class: "small dim" }, "Bring from this server"), bringPick()) : null);
    const same = running && form.shards === running.shards && form.replicas === running.replicas && form.coordinators === running.coordinators && (!running.volume_gb || form.volume_gb === running.volume_gb);
    const vol = running?.volume_gb ? `, volumes of ${fmt.n(running.volume_gb)} GiB (${running.volumeFrom})` : "";
    const start = running ? h("div", { class: "small dim ro-cstart" }, same
      ? [`Starts from this cluster's size${running.name ? ` (${running.name})` : ""}: ${fmt.n(running.shards)} shards × ${running.replicas} copies, ${running.coordinators} ${running.coordinators === 1 ? "coordinator" : "coordinators"}${vol}, a node for about every 4 copies. `,
        h("a", { href: "#", onclick: (e) => { e.preventDefault(); sized = true; Object.assign(form, { shards: 2, replicas: 3, coordinators: 1, nodes: 3, volume_gb: 10 }); replan(); } }, "Start small instead (2 × 3 on 3 nodes, 10 GiB volumes)")]
      : [`This cluster runs ${fmt.n(running.shards)} shards × ${running.replicas} copies and ${running.coordinators} ${running.coordinators === 1 ? "coordinator" : "coordinators"}${vol}. `, h("a", { href: "#", onclick: (e) => { e.preventDefault(); useRunning(); } }, "Start from its size")]) : null;
    if (planErr) { clear(planEl, card({ title: "Plan", body: h("div", { class: "stack" }, start, controls, h("div", { class: "error" }, planErr.message)) })); return; }
    if (!plan) { clear(planEl, card({ title: "Plan", body: h("div", { class: "stack" }, start, controls, h("div", { class: "small dim" }, "Planning…")) })); return; }
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
        start,
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

  /** Tables of this server the new cluster copies in once it is ready (crate::copy::bring_when_ready). */
  function bringPick() {
    const label = form.bring.length ? form.bring.join(", ") : "nothing";
    return h("details", { class: "cloud-bring" },
      h("summary", { class: "btn sm", title: "Once every copy answers, this server copies these tables in, with their schemas (shard and cluster keys too), through the cluster's Kubernetes API" }, label),
      h("div", { class: "cloud-bring-list stack" }, here.map((t) => h("label", { class: "row nw small" },
        h("input", { type: "checkbox", checked: form.bring.includes(t), onchange: (e) => { form.bring = e.target.checked ? [...form.bring, t] : form.bring.filter((x) => x !== t); e.target.closest("details").querySelector("summary").textContent = form.bring.length ? form.bring.join(", ") : "nothing"; drawCreate(); } }),
        h("span", { class: "mono" }, t)))));
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
      h("span", { class: "small dim" }, why ? why : [form.bring.length ? `Once ready, it brings ${form.bring.join(", ")} from this server. ` : "", (form.delete_after
        ? `About ${usd(plan.cost.hour, 2)} an hour, billed to the account whose key is saved here, and it deletes itself after ${hoursLabel(form.delete_after)}: about ${usd(plan.cost.hour * form.delete_after, 2)} in all. This server deletes it, so it must be running then (or it deletes it when it runs again).`
        : `About ${usd(plan.cost.hour, 2)} an hour from then until it is deleted, billed to the account whose key is saved here. This page follows it and deletes it.`)])));
  }

  async function create() {
    const name = plan.cluster_name, p = plan;
    const ok = await typed({
      title: `Create ${name} on ${P.name}?`, name, ok: "Create it",
      body: h("div", { class: "stack" },
        h("p", { class: "dim", style: { margin: 0 } }, form.delete_after
          ? `${p.layout.nodes.length} × ${p.machine.type} in ${p.region}, on ${P.k8s}: about ${usd(p.cost.hour, 2)} an hour. It deletes itself, with its data, after ${hoursLabel(form.delete_after)}: about ${usd(p.cost.hour * form.delete_after, 2)} in all.`
          : `${p.layout.nodes.length} × ${p.machine.type} in ${p.region}, on ${P.k8s}: about ${usd(p.cost.hour, 2)} an hour, ${usd(p.cost.month, 0)} a month, from now until it is deleted.`),
        h("p", { class: "dim", style: { margin: 0 } }, prov === "aws"
          ? "CloudFormation makes the network, the EKS cluster and its nodes (15 to 20 minutes); then this server installs ChloeDB's operator and the cluster, and follows its copies until they answer."
          : "DigitalOcean makes the cluster and its nodes (5 to 10 minutes); then this server installs ChloeDB's operator and the cluster, and follows its copies until they answer."),
        form.bring.length ? h("p", { class: "dim", style: { margin: 0 } }, `Then it copies ${form.bring.join(", ")} from this server into it, with their schemas, through its Kubernetes API.`) : null,
        p.warnings.length ? h("div", { class: "stack cloud-warn" }, p.warnings.map((w) => h("div", { class: "row nw small" }, icon("warn"), h("span", {}, w)))) : null),
    });
    if (!ok) return;
    creating = true; drawCreate();
    try {
      // The machine confirmed, not "smallest" again: the cloud's prices may
      // have moved since the plan.
      const m = await api("/v1/admin/cloud/clusters", { body: { provider: prov, plan: { ...body(), machine: p.machine.type }, confirm: name, ...(form.delete_after ? { delete_after_hours: form.delete_after } : { keep: true }), ...(form.bring.length ? { bring: form.bring } : {}) } });
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

  /** Its command center, through its Kubernetes API: the hub keeps a key of its own for it (crate::hub, adopt), then opens /c/<name>. */
  async function openIt(c, btn) {
    btn.disabled = true; btn.lastChild.textContent = "Opening…";
    try { const x = await api("/v1/admin/hub/adopt", { body: { made: c.id }, hub: true }); location.href = `/c/${encodeURIComponent(x.name)}#/home`; }
    catch (e) { toast(e.message, "bad", 9000); btn.disabled = false; btn.lastChild.textContent = "Open its command center"; }
  }

  /** When it deletes itself: `hours` from now, or null to keep it. */
  async function expiry(c, hours) {
    try { const m = await api(`/v1/admin/cloud/clusters/${encodeURIComponent(c.id)}/expiry`, { body: { hours } }); clusters = clusters.map((x) => (x.id === m.id ? m : x)); drawMade(); toast(hours ? `${c.cluster} deletes itself at ${when(m.delete_at_ms)}` : `${c.cluster} is kept until it is deleted`, "ok"); }
    catch (e) { toast(e.message, "bad", 8000); }
  }

  /** Asks the cloud for its ChloeDB clusters that this server does not follow. */
  async function look() {
    looking = true; foundErr = null; drawFound();
    const r = region || key()?.region || P.region;
    try {
      const q = prov === "aws" ? `provider=aws&region=${encodeURIComponent(r)}` : "provider=digitalocean";
      found = (await api(`/v1/admin/cloud/found?${q}`)).clusters || []; foundRegion = r;
    } catch (e) { foundErr = e; }
    looking = false;
    if (alive) drawFound();
  }

  async function removeFound(c) {
    const ok = await typed({
      title: `Delete ${c.cluster}?`, name: c.cluster, ok: "Delete it",
      body: h("div", { class: "stack" },
        h("p", { class: "dim", style: { margin: 0 } }, `It was not made from this server: made from another one, or forgotten here. This deletes the StrataCluster and its volumes (its data), then ${c.provider === "aws" ? "the stack: the nodes, the EKS cluster and the network" : "the DOKS cluster, its nodes and what it made"}. It cannot be undone.`),
        c.owner ? h("p", { class: "small dim", style: { margin: 0 } }, `Its tag says ${c.owner} asked for it.`) : null),
    });
    if (!ok) return;
    try {
      const m = await api("/v1/admin/cloud/found/delete", { body: { provider: c.provider, cloud_id: c.cloud_id, region: c.region, confirm: c.cluster } });
      clusters = [m, ...clusters.filter((x) => x.id !== m.id)];
      found = (found || []).filter((x) => x.cloud_id !== c.cloud_id);
      toast(`Deleting ${c.cluster}`, "ok");
      drawMade(); drawFound(); poll(2000);
    } catch (e) { toast(e.message, "bad", 8000); }
  }

  function drawFound() {
    if (!may()) { clear(foundEl); return; }
    const r = region || key()?.region || P.region;
    const stale = found && prov === "aws" && foundRegion !== r;
    const where = prov === "aws" ? `the CloudFormation stacks ChloeDB made in ${r}` : "the DOKS clusters ChloeDB made, in every region";
    const lookBtn = h("button", { class: "btn sm", disabled: looking, onclick: look }, icon("search"), looking ? "Looking…" : "Look in the account");
    let body;
    if (foundErr) body = h("div", { class: "stack" }, failure(foundErr, "The account's clusters"), h("div", {}, lookBtn));
    else if (!found || stale) body = h("div", { class: "row between" }, h("span", { class: "small dim" }, `A cluster made from another server, or forgotten here, is not on the list above. This asks ${P.name} for ${where}, and deletes none until you say which.`), lookBtn);
    else if (!found.length) body = h("div", { class: "small dim" }, `None: every ChloeDB cluster in ${prov === "aws" ? r : "the account"} is on this server's list.`);
    else body = h("div", { class: "stack" }, found.map((c) => h("div", { class: "cloud-made-row" },
      h("div", { class: "row between" },
        h("div", { class: "row nw" }, h("b", { class: "mono" }, c.cluster), pill(/^(running|CREATE_COMPLETE|UPDATE_COMPLETE)$/.test(c.state) ? "ok" : /FAILED|error|degraded/i.test(c.state) ? "bad" : "warn", String(c.state || "").toLowerCase().replace(/_/g, " ")),
          h("span", { class: "small dim" }, [c.provider === "aws" ? "AWS" : "DigitalOcean", c.region, c.nodes ? `${c.nodes} × ${c.machine}` : null, c.owner ? `asked for by ${c.owner}` : null].filter(Boolean).join(" · "))),
        c.here
          ? h("span", { class: "row nw" }, pill("warn", "this server runs on it"), h("button", { class: "btn sm danger", disabled: true, title: "Deleting it from here would stop this server halfway: delete it from another server, or from the cloud's console" }, icon("trash"), "Delete"))
          : h("button", { class: "btn sm danger", onclick: () => removeFound(c) }, icon("trash"), "Delete")),
      c.created_at ? h("div", { class: "small dim" }, `Made ${fmt.ago(Date.parse(c.created_at))}`) : null)));
    clear(foundEl, card({
      title: "In the account, not made here", cls: "cloud-made",
      sub: found && !stale ? `${found.length} found` : null,
      tools: found && !stale ? [h("button", { class: "btn sm ghost", title: "Ask again now", disabled: looking, onclick: look }, icon("refresh"))] : [],
      body,
    }));
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
    // One found in the account has no planned price: it is left out.
    const billing = clusters.filter((c) => c.phase !== "deleted" && !c.found).reduce((a, c) => a + c.cost_hour, 0);
    clear(made, card({
      title: "Clusters made here", cls: "cloud-made",
      sub: up ? `${up} up or on the way${billing ? `, about ${usd(billing, 2)} an hour` : ""}` : "none running",
      tools: [h("button", { class: "btn sm ghost", title: "Ask again now", onclick: loadMade }, icon("refresh"))],
      body: h("div", { class: "stack" }, clusters.map(madeRow)),
    }));
  }

  /** What bringing this server's tables did, or that it will. */
  function broughtLine(c) {
    if (!c.bring?.length) return null;
    const b = c.brought, what = c.bring.join(", ");
    if (!b) return ["deleting", "deleted", "failed"].includes(c.phase) ? null : h("div", { class: "small dim" }, icon("import"), ` Brings ${what} from this server once every copy answers.`);
    if (b.error) return h("div", { class: "small error" }, icon("warn"), ` Did not bring ${what}: ${b.error}. Copy them from Connections.`);
    return h("div", { class: "small" }, icon("check"), ` Brought ${what} from this server: ${fmt.n(b.written || 0)} rows.`);
  }

  function madeRow(c) {
    const order = ["creating", "installing", "starting", "ready"];
    const at = order.indexOf(c.phase);
    const stages = [[c.provider === "aws" ? "Stack" : "Cluster", "the cloud makes the cluster and its nodes"], ["ChloeDB", "the operator and the cluster are applied"], ["Copies", "the operator starts every copy"], ["Ready", "every copy answers"]];
    const track = at < 0 ? null : h("div", { class: "cloud-track" }, stages.map(([label, what], i) =>
      h("span", { class: "cloud-stage" + (i < at || c.phase === "ready" ? " done" : i === at ? " now" : ""), title: what }, label)));
    const last = c.steps[c.steps.length - 1];
    const progress = c.progress && c.phase === "creating" ? ` (${c.progress.done} of ${c.progress.of} ${c.provider === "aws" ? "resources" : "nodes"})` : "";
    const action = c.phase === "deleted" || (c.phase === "failed" && !c.cloud_id)
      ? h("button", { class: "btn sm", title: "Take it off this list", onclick: () => forget(c) }, icon("x"), "Forget")
      : c.phase === "deleting" ? null
      : h("button", { class: "btn sm danger", onclick: () => remove(c) }, icon("trash"), "Delete");
    // On the hub's own page: its command center through the cluster's
    // Kubernetes API, nothing to install. (On another cluster's page, that
    // cluster made it, with its own cloud credentials.)
    const open = connectionName ? null
      : c.connection ? h("a", { class: "btn sm primary", href: `/c/${encodeURIComponent(c.connection)}#/home`, title: `Opens at /c/${c.connection}: calls go through its Kubernetes API with a key this server keeps` }, icon("ext"), h("span", {}, "Open its command center"))
      : ["starting", "ready"].includes(c.phase) ? h("button", { class: "btn sm primary", title: "Through its Kubernetes API, with the cloud's credentials here: no public address, no kubectl. A key of this server's own is made there and kept here.", onclick: (e) => openIt(c, e.currentTarget) }, icon("ext"), h("span", {}, "Open its command center"))
      : null;
    const live = !["deleting", "deleted"].includes(c.phase) && !(c.phase === "failed" && !c.cloud_id);
    const left = c.delete_at_ms ? (c.delete_at_ms - Date.now()) / 3_600_000 : null;
    const ends = !live ? null : c.delete_at_ms
      ? h("div", { class: "small cloud-expiry" },
        h("span", {}, icon("clock"), ` Deletes itself at ${when(c.delete_at_ms)} (${left > 0 ? `in ${left >= 1 ? `${left.toFixed(1)} h` : `${Math.max(1, Math.round(left * 60))} min`}` : "now"}), about ${usd(c.cost_by_delete, 2)} in all.`),
        h("span", { class: "row nw", style: { gap: "4px" } },
          h("button", { class: "btn xs", title: "Four hours more than the time left now", onclick: () => expiry(c, Math.min(720, Math.max(0, left) + 4)) }, "+4 h"),
          h("button", { class: "btn xs ghost", title: "It no longer deletes itself", onclick: () => expiry(c, null) }, "Keep it")))
      : h("div", { class: "small dim cloud-expiry" }, h("span", {}, icon("clock"), " Kept until it is deleted."),
        h("button", { class: "btn xs ghost", onclick: (e) => menu(e.currentTarget, AFTER.filter(([v]) => v).map(([v, l]) => ({ label: `Delete it in ${l}`, onclick: () => expiry(c, v) }))) }, "Delete it later…"));
    return h("div", { class: "cloud-made-row" },
      h("div", { class: "row between" },
        h("div", { class: "row nw" }, h("b", { class: "mono" }, c.cluster), pill(...PHASES[c.phase]),
          h("span", { class: "small dim" }, [c.provider === "aws" ? "AWS" : "DigitalOcean", c.region, c.nodes ? `${c.nodes} × ${c.machine}` : null].filter(Boolean).join(" · "))),
        h("div", { class: "row nw" },
          c.found
            ? h("span", { class: "small dim", title: "It was made elsewhere: what it cost is in the cloud's billing" }, "found in the account")
            : h("span", { class: "small dim num", title: "at the planned price, from when it was asked for" }, `${usd(c.cost_so_far, 2)} so far${c.phase === "deleted" ? "" : ` · ${usd(c.cost_hour, 2)}/h`}`),
          open, action)),
      track,
      ends,
      broughtLine(c),
      last ? h("div", { class: "small" }, h("span", { class: "dim" }, fmt.ago(last.at_ms) + " · "), last.text + progress) : null,
      c.summary && !["deleted", "ready"].includes(c.phase) && !(last && last.text.includes(c.summary)) ? h("div", { class: "small" }, h("span", { class: "dim" }, "The StrataCluster: "), c.summary) : null,
      c.phase === "failed" && c.error ? h("div", { class: "error small" }, c.error, c.cloud_id ? " Delete it: what the cloud made may still bill." : "") : null,
      c.connect.length ? h("details", {}, h("summary", { class: "small" }, "Or reach it with kubectl"),
        h("div", { class: "small dim", style: { margin: "6px 0" } }, "Point kubectl at it, forward the coordinator's port, then open http://127.0.0.1:7700 with the cluster's token (the Secret strata-token in the strata namespace)."),
        cmds(c.connect)) : null,
      h("details", {}, h("summary", { class: "small" }, `What happened (${c.steps.length})`),
        h("div", { class: "stack small cloud-steps" }, c.steps.map((st) => h("div", { class: "row nw" }, h("span", { class: "dim num" }, fmt.time(st.at_ms)), h("span", {}, st.text))))));
  }

  load();
  loadMade();
  loadRunning();
  loadHere();
  return () => { alive = false; clearTimeout(pollT); hideTip(); switchProvider = null; };
}
