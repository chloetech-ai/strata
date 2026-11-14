// Review and ship (JOURNEYS.md, section C): three steps.
//   1 What is ready      #/p/<project>/ship
//   2 Check it           #/p/<project>/ship/<agent>
//   3 Merge or send back #/p/<project>/ship/<agent>/merge (opens the merge sheet)
// mount(root, ctx) -> { update(state), unmount() }. ctx is the level ctx
// (api, state, params, go, openWhy, openRewind) plus { project, crumbs, go,
// needs } from the router. Built on the shared pieces of ../levels/common.js;
// every colour is a token. The backend is src/review.rs.

import {
  h, useStyle, icon, kc, label, btn, page, stepper, pane, bank, since, churnBar, churnSquares, riskLabel, blastCompact, repoOf,
} from "../levels/common.js";

const enc = encodeURIComponent;
const plural = (n, w, ws = w + "s") => `${n} ${n === 1 ? w : ws}`;
const short = (s) => String(s || "").slice(0, 7);
const PHONE = "(max-width: 640px)";

/** The route under #/p/<project>/ship. */
function parseRoute() {
  const m = (location.hash || "").match(/^#\/p\/([^/?]+)\/ship(?:\/([^/?]+))?(?:\/(merge))?/);
  return m ? { project: decodeURIComponent(m[1]), agent: m[2] ? decodeURIComponent(m[2]) : null, merge: !!m[3] } : { project: null, agent: null, merge: false };
}

function projectOf(ctx, r) {
  const p = ctx.project || {};
  return p.path || p.name || r.project || (ctx.params && (ctx.params.project || ctx.params.repo)) || repoOf(ctx) || "";
}

function projectName(ctx, r) {
  const p = ctx.project || {};
  const n = p.name || r.project || (ctx.params && (ctx.params.project || ctx.params.repo)) || repoOf(ctx) || "";
  return String(n).split("/").filter(Boolean).pop() || "this project";
}

/** A plain sentence about what is missing, never an error box. */
const none = (text) => h("p", { class: "rv-none" }, text);

/** A diff hunk: line numbers, sign, text; `cur` lines marked. */
export function hunkView(lines, { max = 0 } = {}) {
  const list = max ? (lines || []).slice(0, max) : lines || [];
  return h("div", { class: "rv-hunk", role: "table", "aria-label": "Changed lines" }, list.map((l) => {
    const k = l.kind === "+" ? "add" : l.kind === "-" ? "del" : "ctx";
    return h("div", { class: `hl ${k}${l.cur ? " cur" : ""}`, role: "row" },
      h("span", { class: "ln num" }, l.new_line ?? l.old_line ?? ""),
      h("span", { class: "sg", "aria-label": k === "add" ? "added" : k === "del" ? "removed" : null }, l.kind === " " ? "" : l.kind === "-" ? "−" : l.kind),
      h("code", {}, l.text));
  }));
}

/** A check row in the inspector: a label, the words, a mark that is a shape and a word, not a colour alone. */
function checkRow(name, ok, main, sub) {
  const mark = ok === "info" ? ["info", null, ""] : ok === true ? ["ok", "check", "ok"] : ok === false ? ["bad", "x", "no"] : ["unk", "warn", "not checked"];
  return h("div", { class: `rv-chk ${mark[0]}` },
    h("span", { class: "lbl" }, name),
    h("div", { class: "rv-chk-t" }, h("div", {}, main), sub ? h("div", { class: "s" }, sub) : null),
    h("span", { class: "rv-mark", title: mark[2] || null }, mark[1] ? [icon(mark[1], "i sm"), h("span", { class: "sr" }, mark[2])] : null));
}

export function mount(root, ctx) {
  const el = h("div", { class: "rv" });
  root.append(el);
  let alive = true;
  let route = parseRoute();
  let data = null; // the page's last answer
  let err = null;
  let list = null; // /api/review, for the order and "1 of 3"
  let sheet = null;
  let child = null; // ship.js, mounted on demand
  let result = null; // the last merge or undo, said at the top
  const confirming = new Set();

  const api = (path, opts) => ctx.api(path, opts);
  const go = (hash) => (ctx.go ? ctx.go(hash) : (location.hash = hash));
  const proj = () => projectOf(ctx, route);
  const pname = () => projectName(ctx, route);
  const base = () => `#/p/${enc(route.project || pname())}/ship`;

  const steps = (at, agent) => ({
    list: [
      { label: "What is ready", href: base(), state: at === 1 ? "now" : "done" },
      { label: "Check it", href: agent ? `${base()}/${enc(agent)}` : null, state: at === 2 ? "now" : at > 2 ? "done" : "next" },
      { label: "Merge or send back", href: agent ? `${base()}/${enc(agent)}/merge` : null, state: at === 3 ? "now" : "next" },
    ],
  });

  function crumbs(extra) {
    try { ctx.crumbs && ctx.crumbs([{ label: "Start", href: "#/" }, { label: pname(), href: `#/p/${enc(route.project || pname())}` }, { label: "Review", href: base() }, ...(extra || [])]); } catch { /* the router may not take crumbs */ }
  }

  // ------------------------------------------------------------ keys
  const onKey = (e) => {
    if (!alive) return;
    if (e.key === "Escape" && sheet) { e.preventDefault(); closeSheet(); return; }
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && !sheet) {
      const p = el.querySelector("[data-primary]:not([disabled])");
      if (p) { e.preventDefault(); p.click(); }
    }
  };
  document.addEventListener("keydown", onKey);
  const onHash = () => {
    if (!alive || !el.isConnected) return;
    const r = parseRoute();
    if (!r.project && !/^#\/p\//.test(location.hash)) return;
    const same = r.project === route.project && r.agent === route.agent;
    route = r;
    if (!same) { data = null; err = null; list = null; result = null; load(); } else if (r.merge && data && !sheet) openMerge();
  };
  window.addEventListener("hashchange", onHash);

  // ------------------------------------------------------------ loading
  async function load() {
    const want = route.agent;
    render();
    try {
      if (want) {
        const d = await api(`/api/agents/${enc(want)}/review`);
        if (!alive || route.agent !== want) return;
        data = d; err = null;
        render();
        if (route.merge && !sheet) openMerge();
        // The order and the count, in the background.
        api(`/api/review?repo=${enc(proj() || d.repo || "")}`).then((l) => { if (alive && route.agent === want) { list = l; render(); } }).catch(() => {});
      } else {
        const d = await api(`/api/review?repo=${enc(proj())}`);
        if (!alive || route.agent) return;
        data = d; err = null; render();
      }
    } catch (e) {
      if (!alive) return;
      err = e; data = null; render();
    }
  }

  function render() {
    if (!alive) return;
    const scroll = el.scrollTop;
    el.replaceChildren(route.agent ? oneView() : listView());
    el.scrollTop = scroll;
  }

  // ------------------------------------------------------------ step 1: what is ready
  function listView() {
    crumbs();
    const name = pname();
    if (!data && !err) return page({ kicker: "Review and ship", title: `What is ready in ${name}`, steps: steps(1) }, none("Looking at every agent's branch…"));
    if (err) {
      const why = err.status === 404 ? `Workbench knows no project named ${name}.` : err.status === 0 ? "The Workbench server did not answer, so what is ready is not known." : `Workbench could not look at the branches: ${err.message}.`;
      return page({ kicker: "Review and ship", title: `What is ready in ${name}`, steps: steps(1) }, none(why));
    }
    const ready = data.ready || [];
    const busy = data.not_ready || [];
    const order = data.order || {};
    const allClean = ready.length > 1 && ready.every((r) => r.gate && r.gate.primary === "merge" && r.merge && r.merge.ok);
    const first = ready[0];
    const onto = data.onto || "main";
    const primary = allClean
      ? btn(`Merge ${ready.length === 2 ? "both" : `all ${ready.length}`}, in this order`, { primary: true, kbd: "⌘↵", onclick: () => openMergeMany(ready) })
      : first ? btn(`Review ${first.agent.name}`, { primary: true, kbd: "⌘↵", href: `${base()}/${enc(first.agent.id)}` }) : null;
    if (primary) primary.setAttribute("data-primary", "");

    const rows = ready.map((r) => {
      const t = r.tests || {};
      const testWords = !t.ran ? "no test run" : t.failed ? `${t.failed} fail` : `${t.passed} pass`;
      const testMark = !t.ran ? "unk" : t.failed ? "bad" : "ok";
      const m = r.merge || {};
      const clash = (m.others || []).filter((o) => o.clean === false).map((o) => o.agent.name);
      const mergeWords = m.main && m.main.clean === false ? `conflicts with ${onto}` : m.ok ? "merges cleanly" : clash.length ? `conflicts with ${clash.join(", ")}` : "not checked";
      const mergeMark = m.main && m.main.clean === false ? "bad" : m.ok ? "ok" : "unk";
      return h("a", { class: "rv-row", href: `${base()}/${enc(r.agent.id)}`, "aria-label": `Review ${r.agent.name}` },
        h("span", { class: "o num", title: "merge order" }, String(r.order || "")),
        h("span", { class: "w" }, h("b", {}, r.agent.name), h("span", { class: "t" }, r.title || r.branch), r.uncommitted && r.uncommitted.length ? h("span", { class: "t warn" }, `${plural(r.uncommitted.length, "file")} not committed`) : null),
        h("span", { class: `c ${testMark}` }, icon(testMark === "ok" ? "check" : testMark === "bad" ? "x" : "warn", "i sm"), testWords),
        h("span", { class: `c ${mergeMark}` }, icon(mergeMark === "ok" ? "check" : mergeMark === "bad" ? "x" : "warn", "i sm"), mergeWords),
        h("span", { class: "r" }, riskLabel(r.risk && r.risk.level)),
        h("span", { class: "d" }, churnBar(r.added, r.removed)),
        h("span", { class: "go" }, `Review ${r.agent.name}`, icon("chev", "i sm")));
    });

    const body = [];
    if (result) body.push(resultBanner());
    if (!ready.length) {
      body.push(none(busy.length
        ? `Nothing is ready to review in ${name} yet: ${plural(busy.length, "agent")} still ${busy.length === 1 ? "works" : "work"} on ${busy.length === 1 ? "its branch" : "their branches"}.`
        : `Nothing is ready to review in ${name}: no agent has a branch with changes that are not merged.`));
    } else {
      body.push(h("section", { class: "gp-sec" }, label(["Ready ", h("span", { class: "c num" }, String(ready.length))], h("span", {}, "in merge order")),
        h("div", { class: "pane rv-rows" },
          h("div", { class: "rv-row th", "aria-hidden": "true" }, h("span", { class: "o" }, "#"), h("span", { class: "w" }, "Change"), h("span", { class: "c" }, "Tests"), h("span", { class: "c" }, "Merge"), h("span", { class: "r" }, "Risk"), h("span", { class: "d" }, "Diff"), h("span", { class: "go" })),
          rows)));
    }
    if (busy.length) {
      body.push(h("section", { class: "gp-sec" }, label(["Still working ", h("span", { class: "c num" }, String(busy.length))]),
        h("div", { class: "pane" }, busy.map((b) => h("div", { class: "rv-busy" }, h("b", {}, b.agent.name), h("span", { class: "t" }, b.line), churnBar(b.added, b.removed))))));
    }
    for (const m of data.missing || []) body.push(none(m));

    const insp = [];
    if (ready.length) {
      insp.push(h("section", {}, label("Order"),
        h("ol", { class: "rv-order" }, ready.map((r) => h("li", {}, h("span", { class: "num" }, String(r.order)), h("b", {}, r.agent.name), h("code", {}, r.branch)))),
        order.line ? h("p", { class: "rv-p" }, order.line) : null,
        null));
      const gated = ready.filter((r) => r.gate && r.gate.primary !== "merge");
      if (gated.length) insp.push(h("section", {}, label("Not mergeable yet"), gated.map((r) => h("p", { class: "rv-p" }, h("b", {}, r.agent.name), `: ${r.gate.label}.`))));
      insp.push(h("div", { class: "rv-act" }, primary, ready.length > 1 && !allClean ? h("p", { class: "rv-why" }, "Merge each change from its own page: some need you first.") : null));
    }
    return page({
      kicker: `Review and ship · ${plural(ready.length, "change")} ready`,
      title: ready.length ? `${plural(ready.length, "change")} ready in ${name}` : `What is ready in ${name}`,
      sub: ready.length ? `Tests on each branch, whether it merges into ${onto} cleanly, and the order to merge them in.` : null,
      steps: steps(1),
      inspector: insp.length ? insp : null,
    }, ...body, ready.length ? phoneBar([primary && primary.cloneNode(true)], () => (allClean ? openMergeMany(ready) : go(`${base()}/${enc(first.agent.id)}`))) : null);
  }

  /** The phone's sticky bar: the same actions, once (the inspector's are hidden there). */
  function phoneBar(nodes, onPrimary) {
    const bar = h("div", { class: "rv-bar" }, nodes.filter(Boolean));
    const p = bar.querySelector(".btn.primary");
    if (p && onPrimary && p.tagName === "BUTTON") p.onclick = onPrimary;
    for (const b of bar.querySelectorAll("[data-primary]")) b.removeAttribute("data-primary");
    return bar;
  }

  // ------------------------------------------------------------ step 2: check it
  function oneView() {
    const id = route.agent;
    const name0 = ((ctx.state && ctx.state.agents) || []).find((a) => a.id === id)?.name || "this agent";
    crumbs([{ label: data ? data.agent.name : name0, href: `${base()}/${enc(id)}` }]);
    if (!data && !err) return page({ kicker: "Review and ship", title: name0, steps: steps(2, id) }, none(`Checking ${name0}'s branch: tests, merge, blast radius…`));
    if (err) {
      const why = err.status === 404 ? `Workbench knows no agent ${id}.` : err.status === 409 ? `${err.message}.` : err.status === 0 ? "The Workbench server did not answer, so this change could not be checked." : `This change could not be checked: ${err.message}.`;
      return page({ kicker: "Review and ship", title: name0, steps: steps(2, id) }, none(why), h("p", {}, h("a", { class: "lv-link", href: base() }, "Back to what is ready")));
    }
    const d = data;
    const g = d.gate || {};
    const name = d.agent.name;
    const onto = d.onto || "main";
    const at = list && list.ready ? list.ready.findIndex((r) => r.agent.id === d.agent.id) : -1;
    const kicker = at >= 0 ? `Review and ship · ${at + 1} of ${list.ready.length} ready` : `Review and ship · ${d.state || ""}`;
    const t = d.totals || {};
    const chips = h("div", { class: "rv-chips" },
      h("span", { class: "chip" }, d.branch), h("span", { class: "chip" }, plural(t.files || 0, "file")), h("span", { class: "chip" }, plural(d.turns || 0, "turn")),
      h("span", { class: "chip add" }, `+${t.added || 0}`), h("span", { class: "chip del" }, `−${t.removed || 0}`));

    const v = d.verdict || {};
    const blast = d.blast || {};
    const rings = (blast.rings || []).map((r, i) => ({
      n: r.n, label: ["Functions", "Callers", "Public names"][i] || r.label,
      unit: i === 0 ? "changed" : i === 1 ? "reached" : r.n === 1 ? "thing users see" : "things users see",
      names: r.names, code: r.code,
    }));
    const ex = (blast.exceptions || []).slice();
    const tf = (d.checks && d.checks.tests) || {};
    if (tf.failed && !ex.some((x) => /fail/.test(`${x.text} ${x.sub || ""}`))) ex.unshift({ tone: "crit", text: tf.line });
    const verdictNode = [h("b", {}, v.head || ""), " ", v.text || ""];
    const blastNode = blastCompact({ verdict: verdictNode, tone: v.tone || "warn", rings, exceptions: ex.map((x) => ({ tone: x.tone, text: x.text, sub: x.sub })), tests: tf.failed ? null : blast.tests, title: "Blast radius" });
    // The exceptions that ask for you get their button.
    const exNodes = blastNode.querySelectorAll(".blast-ex .ex");
    ex.forEach((x, i) => {
      if (!x.confirm || !exNodes[i]) return;
      const dec = (d.decisions.matter || []).find((m) => m.id === x.record);
      exNodes[i].classList.add("act");
      exNodes[i].append(dec && dec.confirmed ? h("span", { class: "rv-ok" }, icon("check", "i sm"), "Still right") : h("div", { class: "rv-exb" },
        btn("Still right", { sm: true, disabled: confirming.has(x.record), onclick: () => confirm(x.record) }),
        btn("Change it", { sm: true, ghost: true, href: `#/debug/records?id=${enc(x.record)}` })));
    });

    // Pieces of work, riskiest first.
    const pieces = d.pieces || [];
    const mech = d.mechanical || {};
    const pieceRows = pieces.map((p, i) => pieceRow(p, i));
    const piecesNode = h("section", { class: "gp-sec" },
      label(["Pieces of work ", h("span", { class: "c num" }, String(pieces.length + (d.more_pieces || 0)))], h("span", {}, "Riskiest first")),
      pieces.length ? h("div", { class: "pane rv-pieces" },
        h("div", { class: "rv-pc th", "aria-hidden": "true" }, h("span", {}, "Risk"), h("span", {}, "Piece"), h("span", { class: "r" }, "Files"), h("span", { class: "r" }, "Diff")),
        pieceRows,
        d.more_pieces ? h("div", { class: "rv-pc-more" }, `${plural(d.more_pieces, "smaller piece")} not shown here; they are in the full diff below.`) : null)
        : none("No piece of work to show: the branch changes nothing but mechanical files."),
      mech.files && mech.files.length ? h("p", { class: "rv-mech" }, `Not shown: ${mech.what || plural(mech.files.length, "file")} (+${mech.added || 0} −${mech.removed || 0}). `, h("span", { class: "s" }, mech.files.slice(0, 4).join(", "), mech.files.length > 4 ? ` and ${mech.files.length - 4} more` : "")) : null);

    // All details (ship.js) and the pull request draft.
    const detHost = h("div", { class: "rv-ship" });
    const details = h("details", { class: "gp-sec rv-det", id: "rv-details", ontoggle: (e) => { if (e.target.open) mountShip(detHost); } },
      h("summary", {}, label("All details and the pull request draft", icon("down", "i sm"))), detHost);

    const body = [
      result ? resultBanner() : null,
      lastMergeBanner(d),
      d.checks && d.checks.merge && d.checks.merge.clean === false ? conflictBanner(d) : null,
      blastNode,
      piecesNode,
      details,
      ...(d.missing || []).map((m) => none(m)),
    ];

    // The inspector: checks, the key change, decisions, then the actions.
    const c = d.checks || {};
    const tests = c.tests || {};
    const m = c.merge || {};
    const orderNames = list && list.order && list.order.names && list.order.names.length > 1 ? list.order.names.join(", then ") : null;
    const checks = h("section", {}, label("Checks"),
      checkRow("Tests", tests.ran ? tests.ok : null, tests.ran ? h("span", {}, h("b", { class: "num" }, String(tests.passed)), " pass", tests.failed ? [" · ", h("b", { class: "num bad" }, String(tests.failed)), " fail"] : "") : "No test has run",
        tests.failed ? (tests.failing || []).slice(0, 3).join(", ") : tests.ran ? `on ${d.branch}` : `Ask ${name} to run them, or run them in its terminal.`),
      checkRow("Merge", m.clean, m.clean === false ? `Conflicts with ${onto}` : m.clean ? `Cleanly into ${onto}` : `Not checked`, m.clean === false ? `in ${(m.conflicts || []).slice(0, 3).join(", ")}` : m.clean ? null : m.line),
      c.uncommitted && c.uncommitted.length ? checkRow("Commits", false, `${plural(c.uncommitted.length, "file")} not committed`, c.uncommitted.slice(0, 3).join(", ")) : null,
      orderNames ? checkRow("Order", "info", orderNames, list.order.line && list.order.line !== `Merge ${orderNames}.` ? list.order.line.replace(/^Merge [^.]*\.\s*/, "") : null) : null);
    const key = pieces.find((p) => p.hunk);
    const keyNode = key ? h("section", {}, label("Key change", h("code", { class: "rv-where" }, key.hunk.where || key.hunk.path)), hunkView(key.hunk.lines, { max: 14 })) : null;
    const dm = (d.decisions && d.decisions.matter) || [];
    const routine = (d.decisions && d.decisions.routine) || 0;
    const decNode = dm.length || routine ? h("section", {},
      label(["Decisions ", h("span", { class: "c num" }, String(dm.length))], routine ? h("span", {}, `${routine} routine, folded`) : h("span")),
      dm.map((x) => h("div", { class: "rv-dec" },
        h("p", {}, x.text),
        h("div", { class: "s" }, [x.agent && x.agent.name ? x.agent.name : "an agent", x.ts ? since(x.ts) + " ago" : null, x.why === "replaces" ? "replaces an earlier one" : "a new rule"].filter(Boolean).join(" · ")),
        x.why === "replaces" && x.replaces && x.replaces.text ? h("div", { class: "s was" }, `Was: “${x.replaces.text}”`) : null,
        x.why === "replaces" ? (x.confirmed ? h("span", { class: "rv-ok" }, icon("check", "i sm"), "You confirmed it") : h("div", { class: "rv-exb" }, btn("Still right", { sm: true, onclick: () => confirm(x.id) }), btn("Change it", { sm: true, ghost: true, href: `#/debug/records?id=${enc(x.id)}` }))) : null))) : null;

    const acts = actions(d);
    const insp = [checks, keyNode, decNode, h("div", { class: "rv-act" }, acts.nodes(), acts.why)];
    return page({ kicker, title: name, sub: d.title && d.title !== name ? d.title : null, aside: null, steps: steps(route.merge || (d.last_merge && !d.last_merge.undone) ? 3 : 2, d.agent.id), inspector: insp },
      chips, ...body, h("div", { class: "rv-bar" }, acts.nodes(true), acts.why ? acts.why.cloneNode(true) : null));
  }

  /** Merge, Send back and Open a pull request, by the gate. */
  function actions(d) {
    const g = d.gate || {};
    const name = d.agent.name;
    const onto = d.onto || "main";
    const lm = d.last_merge;
    const merged = lm && !lm.undone;
    let why = null;
    if (!g.can_merge) why = h("p", { class: "rv-why" }, `${g.label}.`, g.unconfirmed ? " A decision that replaces an earlier one waits for you to say it is still right." : "");
    else if (g.anyway) why = h("p", { class: "rv-why" }, `${(g.warn || []).map((w) => w[0].toUpperCase() + w.slice(1)).join("; ")}. Send ${name} back, or merge anyway by typing the branch's name.`);
    const nodes = (phone) => {
      const mergeFirst = g.primary === "merge" && g.can_merge && !merged;
      const mergeBtn = merged ? null : btn(g.can_merge && g.anyway ? "Merge anyway…" : `Merge into ${onto}`, {
        primary: mergeFirst, kbd: mergeFirst && !phone ? "⌘↵" : null, disabled: !g.can_merge, title: g.can_merge ? null : g.label,
        onclick: () => openMerge(),
      });
      const backBtn = btn(`Send ${name} back…`, { primary: !mergeFirst && !merged, kbd: !mergeFirst && !merged && !phone ? "⌘↵" : null, onclick: () => openSendBack() });
      const prBtn = merged || phone ? null : btn("Open a pull request instead", { ghost: true, onclick: () => openDetails() });
      if (merged) {
        const next = btn("Back to what is ready", { primary: true, kbd: phone ? null : "⌘↵", href: base() });
        if (!phone) next.setAttribute("data-primary", "");
        return h("div", { class: "rv-btns" }, next, btn(`Send ${name} back…`, { onclick: () => openSendBack() }));
      }
      for (const b of [mergeBtn, backBtn]) if (b && b.classList.contains("primary") && !phone) b.setAttribute("data-primary", "");
      return h("div", { class: "rv-btns" }, mergeFirst ? [mergeBtn, backBtn] : [backBtn, mergeBtn], prBtn);
    };
    return { nodes, why };
  }

  function openDetails() {
    const det = el.querySelector("#rv-details");
    if (!det) return;
    det.open = true;
    det.scrollIntoView({ behavior: "smooth", block: "start" });
  }

  async function mountShip(host) {
    if (child || !data) return;
    host.replaceChildren(none("Loading the details…"));
    try {
      const m = await import("/levels/ship.js");
      if (!alive) return;
      host.replaceChildren();
      child = m.mount(host, { ...ctx, params: { ...(ctx.params || {}), agent: data.agent.id, repo: data.repo } });
    } catch (e) {
      host.replaceChildren(none(`The details could not be shown: ${e.message}.`));
    }
  }

  function pieceRow(p, i) {
    const open = { v: i === 0 && !matchMedia(PHONE).matches };
    const more = h("div", { class: "rv-pc-x" });
    const fill = () => {
      more.hidden = !open.v;
      row.setAttribute("aria-expanded", String(open.v));
      if (!open.v) return more.replaceChildren();
      const all = h("div", { class: "rv-all", hidden: true }, (p.all || []).map((f) => h("div", {}, h("code", { class: "rv-where" }, f.path), (f.hunks || []).map((hk) => hunkView(hk.lines)))), p.truncated ? none("The rest is cut here; the full diff is under All details.") : null);
      more.replaceChildren(
        h("p", { class: "rv-p s" }, p.words),
        p.hunk ? h("div", {}, h("div", { class: "lbl" }, h("span", {}, "Key change"), h("span", { class: "sp" }), h("code", { class: "rv-where" }, p.hunk.where)), hunkView(p.hunk.lines)) : null,
        p.hunks > 1 ? h("button", { class: "lv-link rv-showall", type: "button", onclick: (e) => { all.hidden = !all.hidden; e.target.textContent = all.hidden ? `Show all ${p.hunks} hunks` : "Show only the key change"; } }, `Show all ${p.hunks} hunks`) : null,
        all);
    };
    const row = h("button", { class: "rv-pc", type: "button", onclick: () => { open.v = !open.v; fill(); } },
      h("span", {}, riskLabel(p.risk)),
      h("span", { class: "w" }, h("b", {}, p.title), h("span", { class: "t" }, (p.files || []).slice(0, 3).join(", "), (p.files || []).length > 3 ? ` +${p.files.length - 3}` : "")),
      h("span", { class: "r num" }, String((p.files || []).length)),
      h("span", { class: "r dd" }, churnSquares(p.added, p.removed), h("span", { class: "num cn" }, `+${p.added} −${p.removed}`)));
    fill();
    return h("div", { class: "rv-pcw" }, row, more);
  }

  function conflictBanner(d) {
    const m = d.checks.merge;
    return h("div", { class: "pane rv-banner crit", role: "status" }, icon("x", "i"),
      h("div", {}, h("b", {}, `${d.branch} conflicts with ${d.onto || "main"}`),
        h("p", {}, `Both change ${(m.conflicts || []).slice(0, 3).join(", ") || "the same lines"}. Merging is off until ${d.agent.name} rebases on ${d.onto || "main"}; Send back says so for you.`)));
  }

  function lastMergeBanner(d) {
    const lm = d.last_merge;
    if (!lm) return null;
    if (lm.undone) return h("div", { class: "pane rv-banner", role: "status" }, icon("rewind", "i"), h("div", {}, h("b", {}, `The merge into ${lm.onto} was undone`), h("p", {}, `${lm.onto} went back to ${short(lm.before)}; the branch ${lm.branch} is unchanged.`)));
    return h("div", { class: "pane rv-banner good", role: "status" }, icon("check", "i"),
      h("div", {}, h("b", {}, `Merged ${lm.branch} into ${lm.onto}`, h("span", { class: "num s" }, ` · ${short(lm.commit)} · ${since(lm.ts)} ago`)),
        h("p", {}, lm.can_undo ? `Undo puts ${lm.onto} back at ${short(lm.before)}, as before the merge.` : lm.pushed ? `It was pushed, so Undo is off: take it back with git revert -m 1 ${short(lm.commit)}.` : `${lm.onto} has moved on since, so Undo is off: take it back with git revert -m 1 ${short(lm.commit)}.`)),
      lm.can_undo ? btn("Undo", { icon: "rewind", onclick: () => undo(d.agent.id) }) : null);
  }

  function resultBanner() {
    const r = result;
    return h("div", { class: `pane rv-banner ${r.ok ? "good" : "crit"}`, role: "status" }, icon(r.ok ? "check" : "x", "i"),
      h("div", {}, h("b", {}, r.title), r.text ? h("p", {}, r.text) : null),
      r.undo ? btn("Undo", { icon: "rewind", onclick: r.undo }) : null,
      btn("Dismiss", { ghost: true, sm: true, onclick: () => { result = null; render(); } }));
  }

  // ------------------------------------------------------------ actions
  async function confirm(record) {
    confirming.add(record);
    render();
    try {
      await api("/api/review/confirm", { method: "POST", body: { record, ok: true } });
      confirming.delete(record);
      await load();
    } catch (e) {
      confirming.delete(record);
      result = { ok: false, title: "Not confirmed", text: e.message };
      render();
    }
  }

  async function undo(agent) {
    try {
      const r = await api("/api/review/undo", { method: "POST", body: { agent } });
      result = { ok: true, title: "Merge undone", text: r.message };
    } catch (e) {
      result = { ok: false, title: "Undo did not happen", text: e.message };
    }
    await load();
  }

  async function undoMany(agents) {
    const said = [];
    for (const a of [...agents].reverse()) {
      try { const r = await api("/api/review/undo", { method: "POST", body: { agent: a } }); said.push(r.message); } catch (e) { said.push(e.message); }
    }
    result = { ok: true, title: "Merges undone", text: said.join(" ") };
    await load();
  }

  // ------------------------------------------------------------ sheets
  function closeSheet() {
    if (!sheet) return;
    sheet.remove();
    sheet = null;
    if (route.merge) { route.merge = false; history.replaceState(null, "", `${base()}/${enc(route.agent)}`); render(); }
  }

  function openSheet(title, sub, ...kids) {
    closeSheet();
    const box = h("div", { class: "rv-sheet", role: "dialog", "aria-modal": "true", "aria-label": title },
      h("div", { class: "rv-sh-hd" }, h("div", {}, h("h2", {}, title), sub ? h("p", {}, sub) : null), btn("", { ghost: true, icon: "x", title: "Close", onclick: () => closeSheet() })),
      h("div", { class: "rv-sh-bd" }, kids));
    sheet = h("div", { class: "rv-scrim", onclick: (e) => { if (e.target === sheet) closeSheet(); } }, box);
    document.body.append(sheet);
    const f = box.querySelector("input,textarea,.btn.primary");
    if (f) setTimeout(() => f.focus(), 0);
    return box;
  }

  /** Step 3: what exactly is merged where, then Merge; Undo after. */
  function openMerge() {
    const d = data;
    if (!d || !d.gate || !d.gate.can_merge) return;
    if (!route.merge) { route.merge = true; history.replaceState(null, "", `${base()}/${enc(d.agent.id)}/merge`); }
    const g = d.gate;
    const onto = d.onto || "main";
    const plan = d.plan || {};
    const commits = d.commits || [];
    const t = d.totals || {};
    const typed = g.anyway ? h("input", { class: "field", placeholder: `type ${d.branch}`, "aria-label": `Type ${d.branch} to merge anyway`, autocomplete: "off", spellcheck: "false", oninput: () => { go1.disabled = typed.value.trim() !== d.branch; } }) : null;
    const status = h("p", { class: "rv-why", role: "status" });
    const go1 = btn(g.anyway ? `Merge ${d.branch} anyway` : `Merge into ${onto}`, { primary: true, disabled: !!g.anyway, onclick: async () => {
      go1.disabled = true; status.textContent = "Merging…";
      try {
        const r = await api("/api/review/merge", { method: "POST", body: { agents: [d.agent.id], anyway: typed ? [typed.value.trim()] : [] } });
        closeSheet();
        result = null; // the merge's own banner says it, with Undo
        await load();
      } catch (e) {
        go1.disabled = false;
        status.textContent = e.code === "conflict" ? `${e.message} Nothing was merged.` : e.code === "dirty" ? `${e.message}` : `Nothing was merged: ${e.message}`;
      }
    } });
    openSheet(`Merge ${d.branch} into ${onto}`, "Exactly what happens, before it happens.",
      h("dl", { class: "kv" },
        h("dt", {}, "From"), h("dd", {}, h("code", {}, d.branch), ` · ${plural(commits.length, "commit")} · ${plural(t.files || 0, "file")} · +${t.added || 0} −${t.removed || 0}`),
        h("dt", {}, "Into"), h("dd", {}, h("code", {}, onto), plan.onto_tip ? ` at ${short(plan.onto_tip)}` : "", ` in ${plan.where || d.repo}`),
        h("dt", {}, "How"), h("dd", {}, plan.onto_checked_out ? `A merge commit, “Merge ${d.branch} (${d.agent.name})”, made in that checkout (it has ${onto} checked out and nothing uncommitted).` : `A merge commit, “Merge ${d.branch} (${d.agent.name})”, written to ${onto} without touching any checkout.`),
        h("dt", {}, "Not done"), h("dd", {}, "Nothing is pushed, and the agent's branch and worktree stay as they are."),
        h("dt", {}, "Undo"), h("dd", {}, `Puts ${onto} back at ${short(plan.onto_tip) || "where it was"}, while nothing was pushed and ${onto} has not moved on.`)),
      commits.length ? h("ul", { class: "rv-commits" }, commits.slice(0, 6).map((c) => h("li", {}, h("code", {}, short(c.sha || c.id || c.commit)), " ", c.subject || c.title || c.message || ""))) : null,
      g.anyway ? h("div", { class: "rv-anyway" }, h("p", {}, `${(g.warn || []).join("; ")}. To merge anyway, type the branch's name:`), typed) : null,
      h("div", { class: "rv-btns" }, go1, btn("Cancel", { onclick: () => closeSheet() })), status);
  }

  function openMergeMany(ready) {
    const onto = (data && data.onto) || "main";
    const status = h("p", { class: "rv-why", role: "status" });
    const go1 = btn(`Merge ${ready.length === 2 ? "both" : `all ${ready.length}`} into ${onto}`, { primary: true, onclick: async () => {
      go1.disabled = true; status.textContent = "Merging…";
      try {
        const r = await api("/api/review/merge", { method: "POST", body: { agents: ready.map((x) => x.agent.id), anyway: [] } });
        closeSheet();
        const ids = (r.merged || []).filter((m) => m.how !== "already").map((m) => m.agent.id);
        result = { ok: true, title: r.message, text: `Undo takes them back in reverse order, while ${onto} is still at these merges and nothing was pushed.`, undo: ids.length ? () => undoMany(ids) : null };
        await load();
      } catch (e) {
        go1.disabled = false;
        status.textContent = `Stopped: ${e.message}`;
        load();
      }
    } });
    sheet = null;
    openSheet(`Merge ${ready.length === 2 ? "both" : `all ${ready.length}`}, in this order`, `Each branch becomes a merge commit on ${onto}, one after the other. Nothing is pushed.`,
      h("ol", { class: "rv-order big" }, ready.map((r) => h("li", {}, h("span", { class: "num" }, String(r.order)),
        h("div", {}, h("b", {}, r.agent.name), " ", h("code", {}, r.branch), h("div", { class: "s" }, `${plural(r.commits || 0, "commit")} · ${plural(r.files || 0, "file")} · +${r.added} −${r.removed} · ${(r.tests && r.tests.line) || ""} · ${(r.merge && r.merge.line) || ""}`))))),
      h("p", { class: "rv-p s" }, `Into ${onto} in ${(data && data.repo) || "the project"}. If one cannot merge, the ones before it stay merged and Workbench says which stopped.`),
      h("div", { class: "rv-btns" }, go1, btn("Cancel", { onclick: () => closeSheet() })), status);
  }

  /** Send back: a note written for you from what is wrong, yours to change. */
  function openSendBack() {
    const d = data;
    if (!d) return;
    const name = d.agent.name;
    const lines = [];
    const c = d.checks || {};
    if (c.merge && c.merge.clean === false) lines.push(`Rebase ${d.branch} on ${d.onto || "main"}: it conflicts in ${(c.merge.conflicts || []).join(", ")}.`);
    if (c.tests && c.tests.failed) lines.push(`These tests fail on ${d.branch}: ${(c.tests.failing || []).join(", ") || `${c.tests.failed} of them`}. Fix them and run the tests again.`);
    const untested = ((d.blast && d.blast.exceptions) || []).find((x) => /^Untested: /.test(x.text));
    if (untested) lines.push(`Add tests for ${untested.text.replace(/^Untested: /, "")}.`);
    if (c.uncommitted && c.uncommitted.length) lines.push(`Commit your changes: ${plural(c.uncommitted.length, "file")} ${c.uncommitted.length === 1 ? "is" : "are"} not committed.`);
    const note = h("textarea", { class: "field", rows: "6", "aria-label": `What ${name} should do` }, lines.join("\n"));
    const status = h("p", { class: "rv-why", role: "status" });
    const send = btn(`Send ${name} back`, { primary: true, onclick: async () => {
      const text = note.value.trim();
      if (!text) { status.textContent = `Say what ${name} should do first.`; return; }
      send.disabled = true; status.textContent = "Sending…";
      try {
        try { await api(`/api/agents/${enc(d.agent.id)}/steer`, { method: "POST", body: { text } }); }
        catch (e) { if (e.status !== 409) throw e; await api(`/api/agents/${enc(d.agent.id)}/restart`, { method: "POST", body: { prompt: text } }); }
        closeSheet();
        result = { ok: true, title: `Sent ${name} back`, text: `It has your note and works on ${d.branch} again. This page updates when it is ready.` };
        render();
      } catch (e) {
        send.disabled = false; status.textContent = `Not sent: ${e.message}`;
      }
    } });
    openSheet(`Send ${name} back`, lines.length ? "Workbench wrote the note from what is wrong; change it as you like." : `Say what ${name} should change.`,
      note, h("div", { class: "rv-btns" }, send, btn("Cancel", { onclick: () => closeSheet() })), status);
  }

  load();
  return {
    update(state) { if (ctx.state && state) ctx.state.agents = state.agents || ctx.state.agents; if (child && child.update) child.update(state); },
    unmount() {
      alive = false;
      document.removeEventListener("keydown", onKey);
      window.removeEventListener("hashchange", onHash);
      if (sheet) sheet.remove();
      if (child && child.unmount) child.unmount();
      el.remove();
    },
    destroy() { this.unmount(); },
  };
}

useStyle("goal-review", `
.rv{min-height:100%;overflow-wrap:anywhere}
.rv .btn[disabled]{background:var(--surface-2);color:var(--text-2);border-color:var(--line);cursor:not-allowed}
.rv-none{margin:var(--s-3) 0;color:var(--text-2);font-size:var(--fs-md);line-height:var(--lh-md);max-width:68ch}
.rv-chips{display:flex;gap:6px;flex-wrap:wrap;margin:calc(-1 * var(--s-4)) 0 var(--s-5)}
.rv .blast-c .verdict p{font-size:var(--fs-lg);line-height:var(--lh-lg);font-weight:var(--w-text);color:var(--text-2)}
.rv .blast-c .verdict p b{color:var(--text);font-weight:var(--w-title)}
.rv .blast-c{margin-top:var(--s-2)}
.rv .ex.act{grid-template-columns:16px minmax(0,1fr) auto;align-items:center}
.rv-exb{display:flex;gap:var(--s-1);flex-wrap:wrap}
.rv-ok{display:inline-flex;align-items:center;gap:var(--s-1);color:var(--good-ink);font-size:var(--fs-sm);font-weight:var(--w-mid)}
.rv-rows{overflow:hidden}
.rv-row{display:grid;grid-template-columns:28px minmax(0,1.4fr) 96px minmax(0,1fr) 44px 170px 150px;gap:var(--s-3);align-items:center;padding:10px var(--s-4);min-height:56px;border-bottom:var(--hair) solid var(--line);color:var(--text);text-decoration:none}
.rv-row:last-child{border-bottom:0}
.rv-row:not(.th):hover{background:var(--surface-2)}
.rv-row.th{min-height:36px;padding-top:var(--s-2);padding-bottom:var(--s-2);font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2)}
.rv-row .o{font-family:var(--mono);color:var(--text-2)}
.rv-row .w{display:flex;flex-direction:column;min-width:0}
.rv-row .w b{font-weight:var(--w-title)}
.rv-row .t,.rv-busy .t{color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.rv-row .t.warn{color:var(--warn-ink)}
.rv-row .c{display:inline-flex;align-items:center;gap:6px;font-size:var(--fs-sm);color:var(--text-2)}
.rv-row .c.ok svg{color:var(--good)}.rv-row .c.bad{color:var(--crit-ink)}.rv-row .c.unk svg{color:var(--text-3)}
.rv-row .go{justify-content:flex-end;display:inline-flex;align-items:center;gap:var(--s-1);color:var(--accent-text);font-weight:var(--w-mid);font-size:var(--fs-sm);white-space:nowrap}
.rv-busy{display:grid;grid-template-columns:minmax(0,10em) minmax(0,1fr) auto;gap:var(--s-3);align-items:center;padding:10px var(--s-4);border-bottom:var(--hair) solid var(--line)}
.rv-busy:last-child{border-bottom:0}
.rv-order{margin:var(--s-3) 0 0;padding:0;list-style:none;display:flex;flex-direction:column;gap:var(--s-2)}
.rv-order li{display:flex;gap:var(--s-2);align-items:baseline;flex-wrap:wrap}
.rv-order li>.num{color:var(--text-2);min-width:1.5em}
.rv-order code{color:var(--text-2);font-size:var(--fs-sm)}
.rv-order.big li{padding:var(--s-2) 0;border-bottom:var(--hair) solid var(--line);flex-wrap:nowrap}
.rv-order .s,.rv-p.s,.rv-dec .s{color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.rv-p{margin:var(--s-2) 0 0;line-height:var(--lh-body)}
.rv-act{margin-top:auto;padding-top:var(--s-4);border-top:var(--hair) solid var(--line);display:flex;flex-direction:column;gap:var(--s-2)}
.rv-btns{display:flex;flex-wrap:wrap;gap:var(--s-2)}
.gp-insp .rv-btns .btn{flex:1 1 40%}
.gp-insp .rv-btns .btn.primary,.gp-insp .rv-btns .btn:first-child{flex-basis:100%;height:var(--ctl-h-lg)}
.gp-insp .rv-btns .btn.ghost{flex-basis:100%}
.rv-why{margin:0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.rv-chk{display:grid;grid-template-columns:88px minmax(0,1fr) 20px;gap:var(--s-3);align-items:center;padding:var(--s-3) 0;border-bottom:var(--hair) solid var(--line)}
.rv-chk:last-child{border-bottom:0}
.rv-chk .s{color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm);margin-top:2px}
.rv-chk .bad{color:var(--crit-ink)}
.rv-mark{display:inline-flex;justify-content:flex-end}
.rv-chk.ok .rv-mark{color:var(--good)}.rv-chk.bad .rv-mark{color:var(--crit)}.rv-chk.unk .rv-mark{color:var(--text-3)}
.sr{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap}
.rv .lbl>span:first-child{flex:none;white-space:nowrap}
.rv .lbl .c{color:var(--text)}
.gp-insp .rv-hunk .hl{min-width:0}.gp-insp .rv-hunk code{white-space:pre-wrap;overflow-wrap:anywhere}
.rv-where{text-align:right;min-width:0;overflow-wrap:anywhere;font-size:var(--fs-xs);color:var(--text-2);text-transform:none;letter-spacing:0;font-weight:var(--w-text)}
.rv-hunk{margin-top:var(--s-2);border:var(--hair) solid var(--line);border-radius:var(--r-sm);background:var(--surface);overflow-x:auto;font:var(--w-text) var(--fs-sm)/var(--lh-sm) var(--mono)}
.rv-hunk .hl{display:grid;grid-template-columns:3.2em 1.4em minmax(0,1fr);min-width:max-content;padding:1px var(--s-2) 1px 0}
.rv-hunk .ln{color:var(--text-3);text-align:right;padding-right:var(--s-2)}
.rv-hunk .sg{color:var(--text-2);text-align:center}
.rv-hunk code{white-space:pre;font-size:inherit}
.rv-hunk .add{background:var(--add-bg)}.rv-hunk .add .sg{color:var(--add-ink)}
.rv-hunk .del{background:var(--del-bg)}.rv-hunk .del .sg{color:var(--del-ink)}
.rv-hunk .cur{box-shadow:inset 3px 0 0 var(--accent)}
.rv-dec{padding:var(--s-3) 0;border-bottom:var(--hair) solid var(--line);display:flex;flex-direction:column;gap:var(--s-1)}
.rv-dec:last-child{border-bottom:0}
.rv-dec p{margin:var(--s-2) 0 0}
.rv-dec .was{font-style:italic}
.rv-pieces{overflow:hidden}
.rv-pc{display:grid;grid-template-columns:56px minmax(0,1fr) 56px 150px;gap:var(--s-3);align-items:center;width:100%;min-height:52px;padding:var(--s-2) var(--s-4);border:0;border-bottom:var(--hair) solid var(--line);background:none;color:var(--text);text-align:left;font:inherit;cursor:pointer}
.rv-pc.th{min-height:36px;cursor:default;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2)}
button.rv-pc:hover{background:var(--surface-2)}
button.rv-pc[aria-expanded=true]{background:var(--surface-2)}
.rv-pc .w{display:flex;flex-direction:column;min-width:0}
.rv-pc .w b{font-weight:var(--w-mid)}
.rv-pc .t{color:var(--text-2);font-size:var(--fs-sm);font-family:var(--mono)}
.rv-pc .r{text-align:right;justify-self:end}
.rv-pc .dd{display:inline-flex;align-items:center;gap:var(--s-2)}
.rv-pc .cn{color:var(--text-2);font-size:var(--fs-xs)}
.rv-pcw:last-child .rv-pc,.rv-pcw:last-child .rv-pc-x{border-bottom:0}
.rv-pc-x{padding:0 var(--s-4) var(--s-4) calc(56px + var(--s-4) + var(--s-3));border-bottom:var(--hair) solid var(--line);background:var(--surface-2)}
.rv-pc-x .lbl{margin-top:var(--s-3)}
.rv-showall{margin-top:var(--s-2)}
.rv-all>div{margin-top:var(--s-3)}
.rv-pc-more{padding:var(--s-3) var(--s-4);color:var(--text-2);font-size:var(--fs-sm)}
.rv-mech{margin:var(--s-2) 0 0;color:var(--text-2);font-size:var(--fs-sm)}
.rv-mech .s{font-family:var(--mono);font-size:var(--fs-xs)}
.rv-det>summary{list-style:none;cursor:pointer;padding:var(--s-3) 0;border-top:var(--hair) solid var(--line);min-height:var(--touch);display:flex;align-items:center}
.rv-det>summary::-webkit-details-marker{display:none}
.rv-det>summary .lbl{flex:1;color:var(--accent-text)}
.rv-ship{margin-top:var(--s-3)}
.rv-banner{display:flex;gap:var(--s-3);align-items:flex-start;padding:var(--s-3) var(--s-4);margin-bottom:var(--s-4);border-left:3px solid var(--text-3)}
.rv-banner>div{flex:1;min-width:0}
.rv-banner p{margin:2px 0 0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.rv-banner .s{color:var(--text-2);font-weight:var(--w-text)}
.rv-banner.good{border-left-color:var(--good)}.rv-banner.good>svg{color:var(--good)}
.rv-banner.crit{border-left-color:var(--crit)}.rv-banner.crit>svg{color:var(--crit)}
.rv-bar{display:none}
.rv-scrim{position:fixed;inset:0;z-index:60;background:var(--scrim);display:grid;place-items:center;padding:var(--s-4)}
.rv-sheet{width:min(var(--sheet-w),100%);max-height:calc(100vh - 32px);overflow:auto;background:var(--surface);border-radius:var(--r);box-shadow:var(--shadow-3);border:var(--hair) solid var(--line)}
.rv-sh-hd{display:flex;gap:var(--s-3);align-items:flex-start;justify-content:space-between;padding:var(--s-4) var(--s-5) 0}
.rv-sh-hd h2{margin:0;font:var(--w-title) var(--fs-lg)/var(--lh-lg) var(--sans);letter-spacing:var(--track-md)}
.rv-sh-hd p{margin:2px 0 0;color:var(--text-2)}
.rv-sh-bd{padding:var(--s-3) var(--s-5) var(--s-5);display:flex;flex-direction:column;gap:var(--s-3)}
.rv-sh-bd .kv{grid-template-columns:72px minmax(0,1fr)}
.rv-commits{margin:0;padding:0;list-style:none;font-size:var(--fs-sm);color:var(--text-2)}
.rv-commits code{color:var(--text)}
.rv-anyway{padding:var(--s-3);border-radius:var(--r-sm);background:var(--warn-soft);color:var(--text)}
.rv-anyway p{margin:0 0 var(--s-2)}
.rv-anyway .field{font-family:var(--mono)}
@media (max-width:1024px){
  .rv-row{grid-template-columns:24px minmax(0,1.4fr) 96px minmax(0,1fr) 44px 140px}
  .rv-row .d{display:none}
}
@media (max-width:640px){
  .rv .btn{white-space:normal;height:auto;min-height:var(--touch);text-align:center;line-height:var(--lh-body);padding-top:var(--s-1);padding-bottom:var(--s-1)}
  .rv .gp-insp .rv-act{display:none}
  .rv .kc{display:none}
  .rv .blast-c>.verdict{order:-2}.rv .blast-c>.lbl{order:-3}.rv .blast-c>.blast-ex{order:-1}
  .rv .blast-c .verdict p{font-size:var(--fs-md);line-height:var(--lh-md)}
  .rv-row{grid-template-columns:24px minmax(0,1fr) auto;grid-template-areas:"o w r" "o c c" "o m m" "o d d";row-gap:var(--s-1);padding:var(--s-3) var(--s-4)}
  .rv-row.th{display:none}
  .rv-row .o{grid-area:o;align-self:start}.rv-row .w{grid-area:w}.rv-row .r{grid-area:r}
  .rv-row .c:nth-of-type(1){grid-area:c}.rv-row .c:nth-of-type(2){grid-area:m}
  .rv-row .d{display:block;grid-area:d}.rv-row .go{display:none}
  .rv-busy{grid-template-columns:minmax(0,1fr)}
  .rv-pc{grid-template-columns:48px minmax(0,1fr);min-height:var(--touch)}
  .rv-pc.th{display:none}
  .rv-pc .r.num{display:none}
  .rv-pc .dd{grid-column:2;justify-self:start}
  .rv-pc-x{padding:0 var(--s-4) var(--s-4)}
  .rv-bar{display:flex;flex-direction:column;gap:var(--s-2);position:sticky;bottom:0;z-index:5;margin:var(--s-5) calc(-1 * var(--s-4)) 0;padding:var(--s-3) var(--s-4) max(var(--s-3),env(safe-area-inset-bottom));background:var(--glass);-webkit-backdrop-filter:var(--glass-blur);backdrop-filter:var(--glass-blur);border-top:var(--hair) solid var(--line)}
  .rv-bar .rv-btns .btn{flex:1 1 40%;height:48px}
  .rv-bar .rv-btns .btn.primary{flex-basis:100%}
  .rv-scrim{place-items:end stretch;padding:0}
  .rv-sheet{width:100%;max-height:92vh;border-radius:var(--r) var(--r) 0 0}
  .rv-sh-bd .kv{grid-template-columns:minmax(0,1fr)}
  .rv-sh-bd .btn{height:48px;flex:1}
  .rv-chk{grid-template-columns:72px minmax(0,1fr) 20px}
}
@media (pointer:coarse){.rv-row,.rv-pc,.rv .btn{min-height:var(--touch)}}
`);
