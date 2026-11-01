// AWS: a Strata cluster on Amazon EKS, planned from this page. The
// account's access key is kept on this server (never shown again, every
// change audited); the region's machines are drawn by price and memory with
// their live prices and the account's quota; the plan lays the shards'
// copies on the nodes, prices them line by line, says what would stop
// them, and hands over the CloudFormation template and the Kubernetes
// manifests, checked by CloudFormation itself. Nothing here creates
// anything in the account.

import { h, s, icon, clear, fmt, card, api, toast, confirm, seg, toggle, pill, empty, table, tipOn, hideTip, debounce, copy, failure, clamp } from "../core.js";

export default {
  id: "aws", title: "AWS", icon: "cloud", group: "Admin",
  keywords: "aws amazon eks cloud kubernetes k8s credentials access key region instance instances machine machines ec2 graviton price prices cost spot quota cloudformation template deploy",
  render,
};

const KINDS = [
  ["general", "General", "var(--s1)"],
  ["memory", "Memory", "var(--s3)"],
  ["compute", "Compute", "var(--s2)"],
  ["storage", "Local disk", "var(--s4)"],
  ["burstable", "Burstable", "var(--s7)"],
];
const kindColor = (k) => (KINDS.find((x) => x[0] === k) || KINDS[0])[2];
const kindLabel = (k) => (KINDS.find((x) => x[0] === k) || [k, k])[1];
const SHARD = (i) => `var(--s${(i % 8) + 1})`;
const HOURS = 730;
const usd = (v, d) => (v == null ? "–" : "$" + (d != null ? v.toFixed(d) : v >= 100 ? fmt.n(v, 0) : v >= 10 ? v.toFixed(1) : v >= 1 ? v.toFixed(2) : v.toFixed(3)));
const gib = (v) => (v == null ? "–" : v >= 1024 ? fmt.n(v / 1024, 1) + " TiB" : (Number.isInteger(v) ? v : v.toFixed(1)) + " GiB");
const graviton = (m) => m.arch === "arm64";
const consoleQuota = (r, spot) => `https://${r}.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/${spot ? "L-34B43A08" : "L-1216C47A"}`;
const REGIONS = ["us-east-1", "us-east-2", "us-west-2", "eu-west-1", "eu-central-1", "eu-north-1", "ap-northeast-1", "ap-southeast-1", "ap-southeast-2", "ap-south-1", "ca-central-1", "sa-east-1"];

/** The permissions this page's calls need: read only. */
const POLICY = {
  Version: "2012-10-17",
  Statement: [{
    Effect: "Allow",
    Action: ["sts:GetCallerIdentity", "ec2:DescribeRegions", "ec2:DescribeInstanceTypes", "ec2:DescribeSpotPriceHistory", "pricing:GetProducts", "servicequotas:GetServiceQuota", "cloudformation:ValidateTemplate"],
    Resource: "*",
  }],
};

function download(name, text, type = "application/json") {
  const a = h("a", { href: URL.createObjectURL(new Blob([text], { type })), download: name });
  document.body.append(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

function stepper(value, min, max, onChange, label, unit = "") {
  const v = h("b", { class: "num" }, String(value) + unit);
  const set = (x) => { x = clamp(x, min, max); if (x !== value) { value = x; v.textContent = String(x) + unit; onChange(x); } };
  return h("div", { class: "aws-step" },
    h("span", { class: "small dim" }, label),
    h("div", { class: "row nw" },
      h("button", { class: "btn xs", "aria-label": "fewer " + label, disabled: value <= min, onclick: () => set(value - (value > 20 && unit ? 10 : 1)) }, icon("minus")),
      v,
      h("button", { class: "btn xs", "aria-label": "more " + label, disabled: value >= max, onclick: () => set(value + (value >= 20 && unit ? 10 : 1)) }, icon("plus"))));
}

function render(page) {
  const head = h("div"), creds = h("div"), where = h("div"), machines = h("div"), planEl = h("div");
  page.append(head, h("div", { class: "aws-top" }, creds, where), machines, planEl);

  let status = null;           // GET /v1/admin/aws
  let regions = null;          // the account's regions
  let region = null;
  let cat = null;              // GET /v1/admin/aws/machines
  let catErr = null, loadingCat = false;
  let editing = false;         // the credentials form is open over saved ones
  const filt = { kind: "all", arch: "any", spot: false, q: "" };
  const form = { name: "search", machine: null, nodes: 3, spot: false, volume_gb: 10, shards: 2, replicas: 3, coordinators: 1, image: "" };
  let plan = null, planErr = null, checked = null, checking = false;
  let seq = 0;

  const have = () => !!status?.credentials;

  async function load() {
    try { status = await api("/v1/admin/aws"); } catch (e) { clear(head); clear(creds, failure(e, "The AWS view")); return; }
    drawHead(); drawCreds();
    if (have()) {
      region = region || status.credentials.region;
      if (!regions) api("/v1/admin/aws/regions").then((r) => { regions = r.regions; drawWhere(); }).catch(() => { regions = null; });
      await loadCatalog(false);
    } else { cat = null; drawWhere(); drawMachines(); drawPlan(); }
  }

  async function loadCatalog(fresh) {
    loadingCat = true; catErr = null; drawWhere(); drawMachines();
    try {
      cat = await api(`/v1/admin/aws/machines?region=${encodeURIComponent(region)}${fresh ? "&fresh=1" : ""}`);
      if (!form.machine || !cat.machines.some((m) => m.type === form.machine)) form.machine = cat.recommended?.find((t) => t.startsWith("m")) || cat.recommended?.[0] || cat.machines[0]?.type || null;
    } catch (e) { catErr = e; cat = null; }
    loadingCat = false;
    drawWhere(); drawMachines(); replan();
  }

  // ---------------------------------------------------------- the header
  function drawHead() {
    const c = status?.credentials;
    clear(head, h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, "AWS"), h("p", {}, "Plan a Strata cluster on Amazon EKS: machines with their prices, where the copies go, what it costs, and the template that makes it.")),
      h("div", { class: "tools" }, c ? pill("ok", c.account ? `account ${c.account}` : "keys from the environment") : pill("pending", "no AWS keys"))));
  }

  // ---------------------------------------------------------- credentials
  function drawCreds() {
    const c = status?.credentials;
    if (c && !editing) {
      clear(creds, card({
        title: "Credentials", sub: c.source === "stored" ? "kept on this server" : "from this server's environment",
        tools: c.source === "stored" ? [
          h("button", { class: "btn sm", onclick: () => { editing = true; drawCreds(); } }, icon("edit"), "Replace"),
          h("button", { class: "btn sm danger", onclick: forget }, icon("trash"), "Forget"),
        ] : [h("button", { class: "btn sm", onclick: () => { editing = true; drawCreds(); } }, icon("plus"), "Save keys here")],
        body: h("div", { class: "stack" },
          h("dl", { class: "kv" },
            h("dt", {}, "Access key"), h("dd", { class: "mono" }, c.access_key_id, c.session_token ? h("span", { class: "small dim" }, " · with a session token") : null),
            c.arn ? [h("dt", {}, "Who"), h("dd", { class: "mono small" }, c.arn)] : null,
            h("dt", {}, "Region"), h("dd", {}, c.region),
            c.saved_ms ? [h("dt", {}, "Saved"), h("dd", {}, `${fmt.ago(c.saved_ms)} by ${c.by}`)] : null,
            status.file ? [h("dt", {}, "Kept in"), h("dd", { class: "mono small" }, status.file)] : [h("dt", {}, "Kept in"), h("dd", { class: "small" }, "memory only: this coordinator forgets them when it stops (--aws-file keeps them)")]),
          h("div", { class: "small dim" }, "The secret never leaves this server: no answer of its API includes it. Saving and forgetting keys are in the audit log.")),
      }));
      return;
    }
    const id = h("input", { class: "input mono", placeholder: "AKIA…", autocomplete: "off", spellcheck: "false" });
    const secret = h("input", { class: "input mono", type: "password", placeholder: "secret access key", autocomplete: "new-password" });
    const token = h("input", { class: "input mono", type: "password", placeholder: "session token (temporary keys only)", autocomplete: "off" });
    const reg = h("select", { class: "select" }, REGIONS.map((r) => h("option", { value: r, selected: r === (c?.region || "us-east-1") }, r)));
    const err = h("div", { class: "small", style: { color: "var(--crit-ink)" } });
    const save = h("button", { class: "btn primary", type: "submit" }, icon("lock"), "Save and check");
    const f = h("form", { class: "stack aws-form", onsubmit: async (e) => {
      e.preventDefault();
      save.disabled = true; err.textContent = "";
      try {
        const body = { access_key_id: id.value.trim(), secret_access_key: secret.value.trim(), region: reg.value };
        if (token.value.trim()) body.session_token = token.value.trim();
        const r = await api("/v1/admin/aws/credentials", { method: "PUT", body });
        secret.value = ""; token.value = "";
        toast(`Saved: ${r.identity?.arn || "the keys"}`, "ok");
        editing = false; regions = null; region = r.credentials?.region || body.region; cat = null;
        await load();
      } catch (ex) { err.textContent = ex.message; }
      save.disabled = false;
    } },
      h("label", { class: "stack aws-field" }, h("span", { class: "small dim" }, "Access key id"), id),
      h("label", { class: "stack aws-field" }, h("span", { class: "small dim" }, "Secret access key"), secret),
      h("details", {}, h("summary", { class: "small dim" }, "Temporary keys (STS)"), h("div", { style: { marginTop: "6px" } }, token)),
      h("label", { class: "stack aws-field" }, h("span", { class: "small dim" }, "Region"), reg),
      err,
      h("div", { class: "row" }, save, editing ? h("button", { class: "btn", type: "button", onclick: () => { editing = false; drawCreds(); } }, "Cancel") : null));
    clear(creds, card({
      title: "Credentials", sub: "an IAM user's access key",
      body: h("div", { class: "stack" },
        f,
        h("div", { class: "small dim" }, "AWS checks them before they are kept (STS GetCallerIdentity). They stay on this server, readable by its user only, and are never answered back; saving and forgetting them go to the audit log."),
        h("details", {}, h("summary", { class: "small" }, "What this page needs (read only)"),
          h("pre", { class: "code small", style: { marginTop: "6px" } }, JSON.stringify(POLICY, null, 2)),
          h("button", { class: "btn xs", onclick: () => copy(JSON.stringify(POLICY, null, 2), "Policy copied") }, icon("copy"), "Copy the policy"),
          h("div", { class: "small dim", style: { marginTop: "6px" } }, "Making the cluster from the template needs more: VPCs, IAM roles and EKS clusters, usually an administrator's."))),
    }));
    setTimeout(() => id.focus(), 0);
  }

  async function forget() {
    if (!(await confirm({ title: "Forget the AWS keys?", body: "This server deletes them. Nothing in the AWS account changes.", ok: "Forget", danger: true }))) return;
    try { await api("/v1/admin/aws/credentials", { method: "DELETE" }); toast("Forgot the AWS keys", "ok"); cat = null; plan = null; await load(); } catch (e) { toast(e.message, "bad"); }
  }

  // ---------------------------------------------------------- region and quota
  function drawWhere() {
    if (!have()) { clear(where, card({ title: "Region and quota", body: empty("cloud", "No AWS keys yet", "Save an access key to see the machines of a region, their prices and your account's quota.") })); return; }
    const list = regions || REGIONS;
    const sel = h("select", { class: "select", onchange: () => { region = sel.value; plan = null; checked = null; loadCatalog(false); } }, list.map((r) => h("option", { value: r, selected: r === region }, r)));
    const q = cat?.quota_vcpus, sq = cat?.spot_quota_vcpus;
    const need = plan?.quota?.needs;
    const meter = (label, have, spot) => {
      const over = need != null && have != null && (spot === form.spot) && need > have;
      const zero = have === 0;
      return h("div", { class: "aws-quota" + (zero || over ? " bad" : "") },
        h("div", { class: "row between" }, h("span", {}, label), h("b", { class: "num" }, have == null ? "unknown" : `${fmt.n(have)} vCPUs`)),
        h("div", { class: "barbox" }, h("div", { class: "bar", style: { width: have ? clamp(((spot === form.spot ? need : 0) || 0) / have * 100, 0, 100) + "%" : "100%", background: zero || over ? "var(--crit)" : "var(--s1)" } })),
        zero ? h("div", { class: "small" }, "The account may start no instances of this kind here. ", h("a", { href: consoleQuota(region, spot), target: "_blank", rel: "noopener" }, "Ask AWS for more"), ", with a use case: new accounts are often refused without one.")
          : over ? h("div", { class: "small" }, `The plan needs ${need}. `, h("a", { href: consoleQuota(region, spot), target: "_blank", rel: "noopener" }, "Ask AWS for more")) : null);
    };
    clear(where, card({
      title: "Region and quota",
      tools: [h("button", { class: "btn sm ghost", title: "Fetch the machines and prices again", disabled: loadingCat, onclick: () => loadCatalog(true) }, icon("refresh"))],
      body: h("div", { class: "stack" },
        h("label", { class: "row" }, h("span", { class: "small dim" }, "Region"), sel),
        loadingCat ? h("div", { class: "small dim" }, "Fetching the machines, prices and quotas from AWS…") : null,
        cat ? [meter("On-demand standard instances", q, false), meter("Spot standard instances", sq, true)] : null,
        cat?.missing?.length ? h("div", { class: "small dim" }, "Not fetched: " + cat.missing.join("; ")) : null,
        cat ? h("div", { class: "small dim" }, `${fmt.n(cat.machines.length)} machines, prices from AWS's price list ${fmt.ago(cat.at_ms)}. EKS ${usd(cat.eks_hour)}/h a cluster, gp3 ${usd(cat.gp3_gb_month, 3)} a GB-month, a public IPv4 address ${usd(cat.ipv4_hour)}/h.`) : null),
    }));
  }

  // ---------------------------------------------------------- machines
  const shown = () => (cat?.machines || []).filter((m) =>
    (filt.kind === "all" || m.kind === filt.kind) &&
    (filt.arch === "any" || (filt.arch === "arm64") === graviton(m)) &&
    (!filt.q || m.type.includes(filt.q.toLowerCase()) || (m.processor || "").toLowerCase().includes(filt.q.toLowerCase())) &&
    (filt.spot ? m.spot_hour : m.usd_hour) != null);
  const price = (m) => (filt.spot ? m.spot_hour : m.usd_hour);

  function select(t) { form.machine = t; drawMachines(); replan(); }

  function chart(list) {
    const W = 760, H = 300, L = 48, B = 28, T = 10, R = 12;
    const xs = list.map(price).filter((v) => v > 0), ys = list.map((m) => m.memory_gib).filter((v) => v > 0);
    if (!xs.length) return empty("info", "No machine matches");
    const lx = [Math.log10(Math.min(...xs)) - 0.05, Math.log10(Math.max(...xs)) + 0.05];
    const ly = [Math.log10(Math.min(...ys)) - 0.05, Math.log10(Math.max(...ys)) + 0.05];
    const X = (v) => L + (Math.log10(v) - lx[0]) / (lx[1] - lx[0]) * (W - L - R);
    const Y = (v) => H - B - (Math.log10(v) - ly[0]) / (ly[1] - ly[0]) * (H - B - T);
    const g = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "aws-chart", role: "img", "aria-label": "Machines by price an hour and memory" });
    for (let e = Math.ceil(lx[0]); e <= Math.floor(lx[1]); e++) for (const k of [1, 2, 5]) {
      const v = k * 10 ** e; if (Math.log10(v) < lx[0] || Math.log10(v) > lx[1]) continue;
      g.append(s("line", { x1: X(v), x2: X(v), y1: T, y2: H - B, class: "aws-grid" }), s("text", { x: X(v), y: H - B + 16, class: "aws-ax", "text-anchor": "middle" }, usd(v)));
    }
    for (let e = Math.ceil(ly[0]); e <= Math.floor(ly[1]); e++) for (const k of [1, 4]) {
      const v = k * 2 ** (e * Math.log2(10)); const vv = Math.round(v); if (vv <= 0) continue;
      if (Math.log10(vv) < ly[0] || Math.log10(vv) > ly[1]) continue;
      g.append(s("line", { x1: L, x2: W - R, y1: Y(vv), y2: Y(vv), class: "aws-grid" }), s("text", { x: L - 6, y: Y(vv) + 4, class: "aws-ax", "text-anchor": "end" }, gib(vv)));
    }
    g.append(s("text", { x: W - R, y: H - 4, class: "aws-ax", "text-anchor": "end" }, filt.spot ? "spot price an hour →" : "on-demand price an hour →"), s("text", { x: 4, y: T + 4, class: "aws-ax" }, "↑ memory"));
    const rec = new Set(cat?.recommended || []);
    // The chosen machine drawn last, over the others.
    const order = list.slice().sort((a, b) => (a.type === form.machine) - (b.type === form.machine));
    for (const m of order) {
      const p = price(m); if (!(p > 0)) continue;
      const on = m.type === form.machine;
      const c = s("circle", {
        cx: X(p), cy: Y(m.memory_gib), r: on ? 7 : 4,
        class: "aws-dot" + (on ? " on" : ""), tabindex: "0", role: "button", "aria-label": `${m.type}: ${m.vcpu} vCPUs, ${gib(m.memory_gib)}, ${usd(p)} an hour`,
        style: { fill: graviton(m) ? kindColor(m.kind) : "var(--surface)", stroke: kindColor(m.kind) },
        onclick: () => select(m.type), onkeydown: (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); select(m.type); } },
      });
      g.append(tipOn(c, () => tip(m)));
      if (rec.has(m.type) && !on) g.append(s("circle", { cx: X(p), cy: Y(m.memory_gib), r: 7.5, class: "aws-rec" }));
    }
    return g;
  }

  function tip(m) {
    return h("div", { class: "stack", style: { gap: "2px" } },
      h("b", {}, m.type, " · ", kindLabel(m.kind)),
      h("div", {}, `${m.vcpu} vCPUs · ${gib(m.memory_gib)}${m.local_gb ? ` · ${fmt.n(m.local_gb)} GB local disk` : ""}`),
      h("div", {}, `${usd(m.usd_hour)}/h on demand (${usd(m.usd_hour * HOURS)} a month) · spot ${usd(m.spot_hour)}/h`),
      h("div", { class: "small dim" }, [m.processor || (graviton(m) ? "AWS Graviton" : "x86-64"), m.network, `${m.max_pods} pods a node on EKS`].filter(Boolean).join(" · ")));
  }

  function drawMachines() {
    if (!have()) { clear(machines); return; }
    if (catErr) { clear(machines, card({ title: "Machines", body: h("div", { class: "error" }, "AWS: " + catErr.message) })); return; }
    if (!cat) { clear(machines, card({ title: "Machines", body: h("div", { class: "small dim" }, loadingCat ? "Fetching the machines of " + region + "…" : "") })); return; }
    const list = shown();
    const rec = new Set(cat.recommended || []);
    const q = h("input", { class: "input sm", placeholder: "Find a type or processor", value: filt.q, oninput: debounce(() => { filt.q = q.value.trim(); drawMachines(); setTimeout(() => { const n = machines.querySelector(".aws-find"); if (n) { n.focus(); n.setSelectionRange(n.value.length, n.value.length); } }, 0); }, 250) });
    q.classList.add("aws-find");
    const tbl = table([
      { key: "type", label: "Type", render: (m) => h("span", { class: "row nw" }, h("span", { class: "aws-sw", style: { background: graviton(m) ? kindColor(m.kind) : "transparent", borderColor: kindColor(m.kind) } }), h("b", { class: "mono" }, m.type), rec.has(m.type) ? pill("ok", "suggested") : null) },
      { key: "kind", label: "For", render: (m) => kindLabel(m.kind) },
      { key: "arch", label: "CPU", render: (m) => (graviton(m) ? "Graviton" : "x86-64") + (m.processor ? "" : ""), sort: (m) => m.processor || m.arch },
      { key: "vcpu", label: "vCPUs", num: true },
      { key: "memory_gib", label: "Memory", num: true, render: (m) => gib(m.memory_gib) },
      { key: "price", label: filt.spot ? "Spot /h" : "On demand /h", num: true, sort: price, render: (m) => usd(price(m)) },
      { key: "month", label: "A month", num: true, sort: price, render: (m) => usd(price(m) * HOURS) },
      { key: "per", label: "$ / GiB-month", num: true, sort: (m) => price(m) * HOURS / m.memory_gib, render: (m) => usd(price(m) * HOURS / m.memory_gib, 2) },
      { key: "local_gb", label: "Local disk", num: true, render: (m) => (m.local_gb ? fmt.n(m.local_gb) + " GB" : "–") },
      { key: "max_pods", label: "Pods", num: true, title: "The pods EKS lets a node of this type run" },
    ], { rows: list, sortKey: "per", desc: false, compact: true, maxHeight: "280px", onRow: (m) => select(m.type), rowClass: (m) => (m.type === form.machine ? "sel" : "") });
    clear(machines, card({
      title: "Machines", sub: list.length === cat.machines.length ? fmt.n(list.length) : `${fmt.n(list.length)} of ${fmt.n(cat.machines.length)}`,
      tools: [
        seg([["all", "All"], ...KINDS.map(([k, l]) => [k, l])], filt.kind, (v) => { filt.kind = v; drawMachines(); }),
        seg([["any", "Any CPU"], ["arm64", "Graviton"], ["x86_64", "x86-64"]], filt.arch, (v) => { filt.arch = v; drawMachines(); }),
        seg([[false, "On demand"], [true, "Spot"]], filt.spot, (v) => { filt.spot = v; drawMachines(); }),
        q,
      ],
      body: h("div", { class: "stack" },
        h("div", { class: "aws-legend small dim" },
          KINDS.map(([k, l, c]) => h("span", { class: "row nw" }, h("span", { class: "aws-sw", style: { background: c, borderColor: c } }), l)),
          h("span", { class: "row nw" }, h("span", { class: "aws-sw", style: { borderColor: "var(--muted)" } }), "hollow: x86-64"),
          h("span", { class: "row nw" }, h("span", { class: "aws-sw ring" }), "ringed: suggested for search (the newest Graviton general and memory machines)")),
        chart(list),
        tbl),
    }));
  }

  // ---------------------------------------------------------- the plan
  const replan = debounce(async () => {
    if (!cat || !form.machine) { drawPlan(); return; }
    const mine = ++seq;
    const body = {
      name: form.name, region, machine: form.machine, nodes: form.nodes, spot: form.spot, volume_gb: form.volume_gb,
      cluster: { shards: form.shards, replicas: form.replicas, coordinators: form.coordinators, ...(form.image ? { image: form.image } : {}) },
    };
    try { const p = await api("/v1/admin/aws/plan", { body }); if (mine !== seq) return; plan = p; planErr = null; }
    catch (e) { if (mine !== seq) return; planErr = e; }
    checked = null;
    drawPlan(); drawWhere();
  }, 200);

  function set(k, v) { form[k] = v; replan(); }

  function nodes() {
    const l = plan.layout, m = plan.machine;
    const box = h("div", { class: "aws-nodes" });
    for (const n of l.nodes) {
      const used = n.copies.length * l.copy_memory_gib;
      box.append(h("div", { class: "aws-node" },
        h("div", { class: "row between small" }, h("b", {}, `node ${n.node + 1}`), h("span", { class: "dim mono" }, m.type)),
        h("div", { class: "aws-copies" }, n.copies.map((c) => h("span", { class: "aws-copy", style: { "--c": SHARD(c.shard) }, title: `shard ${c.shard}, copy ${c.copy + 1}${c.copy ? "" : " (a primary when the cluster starts)"}` }, `s${c.shard}`, h("small", {}, c.copy ? "r" : "p")))),
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
      h("div", { class: "aws-totals" },
        h("div", {}, h("b", { class: "num" }, usd(c.hour, 2)), h("span", { class: "small dim" }, " an hour")),
        h("div", {}, h("b", { class: "num" }, usd(c.hour * 24, 2)), h("span", { class: "small dim" }, " a day")),
        h("div", {}, h("b", { class: "num" }, usd(c.month, 0)), h("span", { class: "small dim" }, " a month"))),
      h("div", { class: "aws-stack" }, lines.map((x, i) => h("span", { style: { width: (x.hour / total * 100) + "%", background: colors[i % colors.length] }, title: `${x.what}: ${usd(x.hour, 3)}/h` }))),
      h("div", { class: "stack", style: { gap: "4px" } }, c.lines.map((x, i) => h("div", { class: "aws-line small" },
        h("span", {}, h("span", { class: "aws-sw", style: { background: colors[i % colors.length], borderColor: colors[i % colors.length] } }), x.what),
        h("span", { class: "num" }, x.hour == null ? "price unknown" : `${usd(x.hour, 3)}/h · ${usd(x.month, 2)}/month`)))),
      other != null && nodesNow != null ? h("div", { class: "small dim" }, form.spot
        ? `On demand, the nodes would cost ${usd(other, 3)}/h, ${fmt.pct(1 - nodesNow / other, 0)} more than spot.`
        : `On spot, the nodes would cost ${usd(other, 3)}/h (${fmt.pct(1 - other / nodesNow, 0)} less), and may be taken back with two minutes' notice.`) : null,
      h("div", { class: "small dim" }, `Not counted: ${c.not_counted}.`));
  }

  async function check() {
    checking = true; drawPlan();
    try {
      const body = { name: form.name, region, machine: form.machine, nodes: form.nodes, spot: form.spot, volume_gb: form.volume_gb, cluster: { shards: form.shards, replicas: form.replicas, coordinators: form.coordinators, ...(form.image ? { image: form.image } : {}) } };
      const r = await api("/v1/admin/aws/validate", { body });
      checked = r.validated;
    } catch (e) { checked = { ok: false, error: e.message }; }
    checking = false; drawPlan();
  }

  function drawPlan() {
    if (!have() || !cat) { clear(planEl); return; }
    const name = h("input", { class: "input sm mono", value: form.name, size: 14, onchange: () => set("name", name.value.trim()) });
    const image = h("input", { class: "input sm mono", value: form.image, placeholder: "ghcr.io/chloetech-ai/strata-server:latest", size: 34, onchange: () => set("image", image.value.trim()) });
    const controls = h("div", { class: "aws-controls" },
      h("label", { class: "aws-step" }, h("span", { class: "small dim" }, "Name"), h("div", { class: "row nw" }, h("span", { class: "small dim mono" }, "strata-"), name)),
      h("div", { class: "aws-step" }, h("span", { class: "small dim" }, "Machine"), h("b", { class: "mono" }, form.machine || "pick one above")),
      stepper(form.nodes, 1, 100, (v) => set("nodes", v), "Nodes"),
      stepper(form.shards, 1, 64, (v) => set("shards", v), "Shards"),
      stepper(form.replicas, 1, 9, (v) => set("replicas", v), "Copies a shard"),
      stepper(form.coordinators, 1, 9, (v) => set("coordinators", v), "Coordinators"),
      stepper(form.volume_gb, 1, 16384, (v) => set("volume_gb", v), "Volume a copy", " GiB"),
      h("div", { class: "aws-step" }, h("span", { class: "small dim" }, "Nodes bought"), toggle(form.spot, (v) => set("spot", v), form.spot ? "spot" : "on demand")),
      h("label", { class: "aws-step" }, h("span", { class: "small dim" }, "Image"), image));
    if (planErr) { clear(planEl, card({ title: "Plan", body: h("div", { class: "stack" }, controls, h("div", { class: "error" }, planErr.message)) })); return; }
    if (!plan) { clear(planEl, card({ title: "Plan", body: h("div", { class: "stack" }, controls, h("div", { class: "small dim" }, "Planning…")) })); return; }
    const stack = plan.stack;
    const files = [
      [stack + ".json", "the CloudFormation template", () => download(stack + ".json", JSON.stringify(plan.template, null, 2))],
      [stack + "-operator.yaml", "the operator", () => download(stack + "-operator.yaml", plan.manifests.operator, "text/yaml")],
      [stack + "-cluster.yaml", "the namespace, a gp3 storage class and the StrataCluster", () => download(stack + "-cluster.yaml", plan.manifests.cluster, "text/yaml")],
    ];
    const cmds = (list) => h("div", { class: "aws-cmds" }, list.map((c) => h("div", { class: "row nw" }, h("code", { class: "mono small" }, c), h("button", { class: "btn xs ghost", title: "Copy", onclick: () => copy(c) }, icon("copy")))));
    const q = plan.quota;
    clear(planEl, card({
      title: "Plan", sub: `${plan.stack} in ${plan.region}`,
      tools: [h("button", { class: "btn sm", disabled: checking, onclick: check, title: "CloudFormation's own check of the template: free, and makes nothing" }, icon("check"), checking ? "Checking…" : "Check with AWS")],
      body: h("div", { class: "stack" },
        controls,
        h("div", { class: "aws-plan" },
          h("div", { class: "stack" },
            h("h4", {}, "Where the copies go"),
            nodes(),
            h("div", { class: "small dim" }, `${plan.layout.pods_per_node} pods on the busiest node, of the ${plan.layout.max_pods} EKS lets a ${plan.machine.type} run. Each node also runs EKS's own pods; the coordinators and the operator go where there is room.`)),
          h("div", { class: "stack" },
            h("h4", {}, "What it costs"),
            costBar(),
            h("div", { class: "row small" }, q.vcpus != null && q.needs > q.vcpus ? pill("bad", "over quota") : pill("ok", "within quota"), `${q.needs} vCPUs of ${q.kind} instances; the account may run ${q.vcpus == null ? "an unknown number" : fmt.n(q.vcpus)} in ${plan.region}.`))),
        plan.warnings.length ? h("div", { class: "stack aws-warn" }, plan.warnings.map((w) => h("div", { class: "row nw small" }, icon("warn"), h("span", {}, w)))) : null,
        checked ? (checked.ok
          ? h("div", { class: "row small" }, pill("ok", "CloudFormation accepts the template"), checked.answer?.capabilities?.length ? `It needs ${checked.answer.capabilities.join(", ")}: it makes IAM roles.` : "")
          : h("div", { class: "row small" }, pill("bad", "CloudFormation refused the template"), checked.error)) : null,
        h("h4", {}, "Make it"),
        h("div", { class: "row" }, files.map(([f, what, go]) => h("button", { class: "btn sm", title: "Download " + what, onclick: go }, icon("download"), f))),
        h("div", { class: "small dim" }, "With the AWS CLI and kubectl, from the folder the files are in. The nodes pull the image above, so it must be one they can reach."),
        cmds(plan.commands),
        h("details", {}, h("summary", { class: "small" }, "Delete it"),
          h("div", { class: "small dim", style: { margin: "6px 0" } }, "The copies' volumes belong to the EBS driver, not to the stack: delete their claims first, or they stay and cost."),
          cmds(plan.teardown))),
    }));
  }

  load();
  return () => hideTip();
}
