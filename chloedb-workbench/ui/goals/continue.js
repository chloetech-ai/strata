// B. Continue a project (JOURNEYS.md): Since you left (`#/p/<p>`),
// Understand (`#/p/<p>/understand`) and Start a change (`#/p/<p>/change`),
// three places under one row of steps, with Review and ship beside them.
// `#/p` opens a folder.
import { h, useStyle, page, btn, label, kc, pane, stateDot, since, ago, fmtClock, avatar, churnBar, toMs, GROUP_WORDS, icon } from "/levels/common.js";
import { repoPicker } from "/levels/repos.js";
import { groupOf, fleet, fleetWords, ORDER, sheet } from "/goals/shell.js";
import { planFrom } from "/goals/newproject.js";

const enc = encodeURIComponent;

/** The project's row of steps: places, not an order. */
export function projectSteps(p, now) {
  const ready = (p.counts && p.counts.ready) || 0;
  const tab = (k, href, text) => h("a", { href, "aria-current": k === now ? "page" : null, class: k === now ? "on" : null }, text);
  return h("nav", { class: "g-psteps", "aria-label": p.project },
    tab("continue", `#/p/${p.project}`, "Since you left"),
    tab("understand", `#/p/${p.project}/understand`, "Understand"),
    tab("change", `#/p/${p.project}/change`, "Start a change"),
    // Health: the tests the agents ran, the alert rules and incidents.
    tab("tests", "#/debug/failing?tab=tests", "Tests and alerts"),
    h("span", { class: "sp" }),
    h("a", { href: `#/p/${p.project}/ship`, class: `g-ship${now === "ship" ? " on" : ""}` }, `Review and ship${ready ? ` (${ready} ready)` : ""}`, icon("chev", "i sm")));
}

/** An answer of `POST /api/ask` (src/ask.rs) as cards, each with where it
 *  came from: one sentence, then memory (who said it, when), the code
 *  (file and line, and the lines), and the history (commits). Never the
 *  raw context, never "null". */
export function answerView(ctx, p, q, r) {
  const s = (v) => (v == null ? "" : String(v));
  const mem = (r.memory || []).filter((m) => m && s(m.text).trim());
  const code = (r.code || []).filter((c) => c && c.path);
  const hist = (r.history && r.history.commits) || [];
  const card = (title, n, kids, more) => h("section", { class: "g-acard" }, label(title, n != null ? h("span", { class: "num" }, String(n)) : null, more || null), kids);
  const took = Number(r.took_ms != null ? r.took_ms : r.ms);
  return h("div", { class: "g-ansv" },
    h("p", { class: "g-asum" }, s(r.summary) || (mem[0] ? s(mem[0].text) : "Nothing in memory, the code or its history matches these words. Name a file, a function or a value.")),
    mem.length ? card("From memory", mem.length, mem.map((m) => h("div", { class: "g-arec" },
      h("div", { class: "g-ameta" }, h("b", {}, s(m.what) || "Noted"), " · ", s(m.who) || "an earlier session", m.ts_ms || m.ts ? ` · ${ago(m.ts_ms || m.ts)}` : "",
        m.id ? h("a", { class: "g-asrc", href: `#/debug/records?id=${enc(m.id)}` }, "ChloeCtx record") : null),
      h("div", { class: m.transcript ? "g-atext q" : "g-atext" }, s(m.text)),
      (m.files || []).length ? h("div", { class: "chips" }, m.files.slice(0, 4).map((f) => h("code", { class: "chip" }, f))) : null))) : null,
    code.length ? card("In the code", code.length, code.map((c) => h("div", { class: "g-acode" },
      h("div", { class: "g-ameta" }, h("code", {}, `${c.path}:${c.line || c.start_line || 1}`), c.symbol ? h("span", { class: "g-dim" }, ` ${s(c.kind) || "in"} ${c.symbol}`) : null,
        h("a", { class: "g-asrc", href: `#/debug/code?q=${enc(c.symbol ? `${c.symbol} ${c.path}` : c.path)}` }, "Why is it like this?")),
      c.snippet ? h("pre", { class: "g-asnip" }, s(c.snippet)) : null))) : null,
    hist.length || (r.history && r.history.answer) ? card("In its history", hist.length || null, [
      r.history && r.history.answer && s(r.history.answer) !== s(r.summary) ? h("p", { class: "g-atext" }, s(r.history.answer)) : null,
      ...hist.map((c) => h("div", { class: "g-acommit" }, h("code", {}, s(c.short) || s(c.sha).slice(0, 7)), h("span", {}, s(c.subject)), h("span", { class: "g-dim" }, [s(c.by), c.ts_ms || c.ts ? ago(c.ts_ms || c.ts) : ""].filter(Boolean).join(" · "))))],
      h("a", { class: "g-asrc", href: `#/debug/code?q=${enc(q)}` }, "Trace it")) : null,
    (r.missing || []).filter(Boolean).length ? h("p", { class: "g-dim" }, r.missing.filter(Boolean).join(" · ")) : null,
    Number.isFinite(took) ? h("p", { class: "g-dim num" }, `Answered in ${Math.max(1, Math.round(took))} ms from memory, the code and git · no model`) : null);
}

/** The ask box: built once and kept across redraws, so a draw never takes
 *  the focus or the question away (Lee: a redraw cleared the question,
 *  and his next keys went to the page's shortcuts). The button and Enter
 *  both ask; the answer shows under the box. */
function askBox(ctx, p, { big, q0, placeholder }) {
  const input = h("input", { class: "field", value: q0 || "", placeholder, "aria-label": `Ask about ${p.project}`, spellcheck: "false" });
  const go = btn("Ask", { type: "submit", sm: !big, primary: !!big });
  const out = h("div", { class: "g-ans", hidden: true, "aria-live": "polite" });
  let n = 0;
  const ask = async () => {
    const q = input.value.trim();
    if (!q) { input.focus(); return; }
    const mine = ++n;
    go.disabled = true;
    go.replaceChildren(h("span", { class: "spin" }), "Asking…");
    out.hidden = false;
    out.replaceChildren(h("p", { class: "g-dim g-asking" }, h("span", { class: "spin" }), `Looking in ChloeCtx's memory, the code and git for “${q}”…`));
    try {
      const r = await ctx.api("/api/ask", { method: "POST", body: { repo: p.path, question: q } });
      if (mine === n) out.replaceChildren(answerView(ctx, p, q, r || {}));
    } catch (e) { if (mine === n) out.replaceChildren(h("p", { class: "g-bad" }, `Not answered: ${e.message}`)); }
    if (mine === n) { go.disabled = false; go.replaceChildren("Ask"); }
  };
  const form = h("form", { class: `g-ask${big ? " big" : ""}`, onsubmit: (e) => { e.preventDefault(); ask(); } }, icon("search", "i"), input, go);
  if (q0) queueMicrotask(ask);
  return { el: h("div", { class: "g-askw" }, form, out), input };
}

/** Kept for other pages: the same answer, in a sheet. */
export async function askSheet(ctx, p, q) {
  const body = h("div", { class: "g-ans" }, h("p", { class: "g-dim g-asking" }, h("span", { class: "spin" }), "Looking in memory, the code and git…"));
  sheet({ kicker: `Ask about ${p.project}`, title: q, body, actions: (done) => [btn("Close", { onclick: done }), btn("Trace it", { href: `#/debug/code?q=${enc(q)}`, onclick: done })] });
  try { body.replaceChildren(answerView(ctx, p, q, (await ctx.api("/api/ask", { method: "POST", body: { repo: p.path, question: q } })) || {})); }
  catch (e) { body.replaceChildren(h("p", { class: "g-bad" }, `Not answered: ${e.message}`)); }
}

export function mount(root, ctx) {
  useStyle("g-proj", CSS);
  const el = h("div", { class: "g-proj" });
  root.append(el);
  const step = ctx.params.page;
  const S = { digest: null, decisions: null, rules: null, views: [], draft: null, sending: false, guarded: null, flaky: null, route: null };
  const ASK = {}; // the ask boxes, kept across draws
  let alive = true;

  const p = ctx.project;
  if (step === "open" || !p) {
    ctx.crumbs([{ label: "Start", href: "#/" }, { label: ctx.params.project || "Open a project", href: location.hash }]);
    el.append(page({ kicker: "Projects", title: ctx.params.project ? `No project named ${ctx.params.project}` : "Open a project", sub: "A git checkout: Workbench reads it without an agent, and agents work in their own worktrees of it." },
      openList(ctx), h("div", { class: "gp-sec" }, picker(ctx))));
    return { update() {}, unmount: () => el.remove() };
  }
  const crumbs = { continue: "Since you left", understand: "Understand", change: "Start a change" };
  ctx.crumbs([{ label: "Start", href: "#/" }, { label: p.project, href: `#/p/${p.project}` }, ...(step === "continue" ? [] : [{ label: crumbs[step], href: location.hash }])]);

  // ----------------------------------------------------------- data
  const load = () => {
    if (step === "continue") {
      ctx.api(`/api/digest?repo=${enc(p.path)}`).then((d) => { S.digest = d; draw(); }, () => {});
      ctx.api(`/api/decisions?repo=${enc(p.path)}&limit=30`).then((d) => { S.decisions = d; draw(); }, () => { S.decisions = { decisions: [] }; draw(); });
    }
    if (step === "understand" || step === "change") ctx.api(`/api/rules?repo=${enc(p.path)}`).then((r) => { S.rules = r.rules || []; draw(); }, () => { S.rules = []; draw(); });
    if (step === "change") {
      ctx.api(`/api/guarded?repo=${enc(p.path)}`).then((r) => { S.guarded = (r.rules || []).filter((x) => x.on); draw(); }, () => { S.guarded = []; });
      ctx.api(`/api/tests?repo=${enc(p.path)}&days=14`).then((r) => { S.flaky = (r.tests || []).filter((t) => t.flaky && !t.gone); if (F.what) F.check(); }, () => { S.flaky = []; });
    }
  };

  /** "I'm stepping away": asks the browser for notifications (after this
   *  click), and says plainly what will reach you and what will not. */
  async function stepAway() {
    const can = typeof Notification !== "undefined";
    let perm = can ? Notification.permission : "unsupported";
    if (can && perm === "default") { try { perm = await Notification.requestPermission(); } catch { /* treated as no */ } }
    try {
      if (perm === "granted") await ctx.api("/api/notify/settings", { method: "PUT", body: { desktop: true } });
      const r = await ctx.api("/api/away/leave", { method: "POST", body: {} });
      const soon = ((r && r.agents) || []).filter((w) => w.level === "now" || w.level === "soon").map((w) => w.name);
      const likely = soon.length ? ` Likely to need you: ${soon.slice(0, 3).join(", ")}.` : "";
      ctx.toast(perm === "granted"
        ? `Marked. While this tab stays open, this computer notifies you when an agent asks or is blocked; nothing goes to your phone or email.${likely}`
        : perm === "denied"
          ? `Marked, but this browser blocks notifications for this page: nothing will reach you while you are away. Allow them in its site settings, or check back here.${likely}`
          : `Marked. This browser cannot notify you from here, so nothing will reach you while you are away; Since you left counts from now.${likely}`);
      ctx.notify && ctx.notify();
      load();
    } catch (e) { ctx.toast(e.message, true); }
  }

  // ----------------------------------------------------------- Since you left
  function sinceSentence(list) {
    const d = S.digest;
    // One mark, the digest's (the header's and Notifications' too): none on
    // a first look, and "here all along" when the page was in use.
    const first = !!d && (d.mode === "first" || !d.since_ms);
    const when = !d ? "Now" : first ? `Your first look at ${p.project}` : d.mode === "here" ? `While you were here, since ${fmtClock(d.since_ms)}` : `Since you left at ${fmtClock(d.since_ms)}, ${ago(d.since_ms)}`;
    const c = list.map((a) => [a, groupOf(a)]);
    const merged = (((ctx.projectByName && p && ctx.projectByName(p.project)) || p || {}).merged) || {};
    const bits = [];
    for (const [a, g] of c) {
      if (g === "needs") bits.push(`${a.name} waits on you`);
      else if (g === "ready") bits.push(`${a.name} is ready to review`);
      else if (g === "working") bits.push(`${a.name} is working`);
      else if (merged[a.id] && Date.now() - (merged[a.id].ts || 0) < 86400000) bits.push(`${a.name} was merged into ${merged[a.id].onto || "main"}`);
      else if (g === "idle") bits.push(`${a.name} is idle, its turn over`);
    }
    if (d && !first) {
      for (const m of d.merged || []) bits.push(`${m.name || "an agent"}'s ${m.branch || "branch"} was merged${m.onto ? ` into ${m.onto}` : ""}`);
      const live = new Set(c.filter(([, g]) => g !== "finished").map(([a]) => a.id));
      for (const e of d.ended || []) if (!live.has(e.agent)) bits.push(`${e.name} ended ${ago(e.ended_ms)}`);
    }
    const done = d ? (d.done || []).length : 0;
    if (done) bits.push(`${done} outcome${done === 1 ? "" : "s"} done`);
    if (!bits.length) return first ? `${when}: no agent has worked here yet.` : `${when}: nothing happened.`;
    return `${when}: ${bits.slice(0, 4).join(", ")}${bits.length > 4 ? `, and ${bits.length - 4} more` : ""}.`;
  }

  function agentRow(a, g, compact) {
    const live = a.state === "waiting" ? a.waiting_for || "waits for you" : a.last_event || a.task || "";
    const r = ((p.ready || []).find((x) => x.id === a.id)) || null;
    const acts = g === "ready" ? btn("Review and ship", { sm: true, href: `#/p/${p.project}/ship/${a.id}` })
      : g === "finished" ? [btn("Restart with the same task", { sm: true, onclick: () => ctx.restart(a.id) })] : null;
    return h("div", { class: `g-arow${compact ? " compact" : ""}`, "data-nav": "", tabindex: "0", "data-href": `#/a/${a.id}` },
      avatar(a, 20),
      h("a", { class: "nm", href: `#/a/${a.id}` }, a.name),
      stateDot(g),
      h("span", { class: "live" }, g === "finished" ? `${GROUP_WORDS.finished} ${ago(a.ended || a.state_since)}${a.task ? " · " + a.task : ""}` : live),
      r ? churnBar(r.added, r.removed) : h("span"),
      h("span", { class: "acts" }, acts));
  }

  function continuePage() {
    const N = ctx.needs();
    const mine = N.items.filter((i) => i.repo === p.path);
    const list = fleet(p.path);
    const compact = ctx.prefs.get("compact");
    const groups = ORDER.filter((g) => g !== "needs").map((g) => [g, list.filter((a) => groupOf(a) === g)]);
    if (!ASK.c) ASK.c = askBox(ctx, p, { placeholder: `Ask about ${p.project}: why does the total round half up now?` });
    const ready = list.filter((a) => groupOf(a) === "ready").length;
    const changed = (p.ready || []).filter((r) => r.files);
    const decs = (S.decisions && S.decisions.decisions) || [];
    const first = !S.digest || S.digest.mode === "first";
    const sinceMs = S.digest && !first ? S.digest.since_ms : 0;
    const newDecs = decs.filter((d) => (toMs(d.ts) || 0) >= sinceMs && !d.superseded);
    const byWho = (d) => (d.by === "you" ? (d.agent ? `you, told to ${d.agent.name}` : "you") : d.agent ? (d.confirmed ? `${d.agent.name}, you confirmed it` : `${d.agent.name}, on its own`) : "an earlier session");
    const insp = [
      h("section", {}, label("What changed", h("span", { class: "num" }, String(changed.length))),
        changed.length ? changed.map((r) => h("a", { class: "g-chg", href: `#/p/${p.project}/ship/${r.id}` }, h("code", {}, r.branch || r.name), h("span", { class: "g-dim" }, `${r.files} file${r.files === 1 ? "" : "s"}`), churnBar(r.added, r.removed)))
          : h("p", { class: "g-dim" }, "No branch has changes waiting.")),
      h("section", {}, label(first ? "Decisions so far" : "Decisions made", h("span", { class: "num" }, String(newDecs.length))),
        !S.decisions ? h("p", { class: "g-dim" }, "Reading ChloeCtx…") : newDecs.length ? newDecs.slice(0, 3).map((d) => h("div", { class: "g-dec" }, h("span", { class: "lv-ctx" }, ""), h("div", {}, h("div", {}, d.words || d.text), h("small", {}, byWho(d), " · ", ago(d.ts))))) : h("p", { class: "g-dim" }, first ? "None yet." : "None since you left."),
        newDecs.length > 3 ? h("a", { class: "lv-link", href: "#/debug/records" }, `All ${newDecs.length} →`) : null),
      h("section", {}, btn("I'm stepping away: notify me", { onclick: stepAway })),
      ctx.slot("rules-suggest"),
    ];
    return page({ kicker: "Project", title: `Continue ${p.project}`, sub: sinceSentence(list), aside: ready && !mine.length ? btn(`Review and ship (${ready})`, { primary: true, href: `#/p/${p.project}/ship`, kbd: "g r" }) : null, inspector: insp, wide: true },
      projectSteps(p, "continue"), ASK.c.el,
      h("div", { class: "gp-sec" }, label(fleetWords(list))),
      mine.length ? h("div", { class: "g-needs" }, label("Needs you", h("span", { class: "num" }, String(mine.length))), mine.map((it) => ctx.needCard(it))) : null,
      groups.filter(([, l]) => l.length).map(([g, l]) => h("div", { class: "gp-sec" }, label(GROUP_WORDS[g], h("span", { class: "num" }, String(l.length))), g === "finished" && !compact
        ? h("details", { class: "g-fold" }, h("summary", {}, `${l.length} finished · show`), h("div", { class: "pane" }, l.map((a) => agentRow(a, g, compact))))
        : h("div", { class: "pane" }, l.map((a) => agentRow(a, g, compact))))),
      !list.length ? h("div", { class: "gp-sec pane g-empty" }, h("b", {}, "No agents in this project yet."), h("p", {}, "Read it first under Understand, or start one."), btn("Start a change", { primary: true, href: `#/p/${p.project}/change`, kbd: "n" })) : null);
  }

  // ----------------------------------------------------------- Understand
  let mapHost = null;
  function understandPage() {
    if (!ASK.u) ASK.u = askBox(ctx, p, { big: true, q0: (ctx.params.query && ctx.params.query.get("q")) || "", placeholder: "Why does the total round half up? Where is tax computed?" });
    if (!mapHost) {
      mapHost = h("div", { class: "g-map" });
      import("/levels/map.js").then((m) => { if (alive) S.views.push((m.mountBare || m.mount)(mapHost, ctx.levelCtx({ repo: p.path }))); }).catch((e) => mapHost.replaceChildren(h("p", { class: "g-bad" }, `The map did not load: ${e.message}`)));
    }
    const rules = S.rules || [];
    const insp = [
      h("section", {}, label("Agents are told", h("a", { href: "#/more/rules" }, "Edit")),
        !S.rules ? h("p", { class: "g-dim" }, "Reading the rules…") : rules.length ? rules.slice(0, 6).map((r) => h("div", { class: "g-rule" }, r.text)) : h("p", { class: "g-dim" }, "No rules yet. A rule is a decision agents are told before they start.")),
      h("section", {}, label("History"), h("p", { class: "g-dim" }, "When, why and by whom a line changed."), btn("Why the code is like this", { href: "#/debug/code" })),
    ];
    return page({ kicker: p.project, title: "Understand", sub: "Nothing here changes code or starts an agent.", inspector: insp, wide: true },
      projectSteps(p, "understand"),
      ASK.u.el,
      h("div", { class: "gp-sec" }, label("Who is changing which file"), mapHost));
  }

  // ----------------------------------------------------------- Start a change
  const F = { what: null };
  const DRAFT = `wb-draft:${p.project}`;
  const readDraft = () => { try { return JSON.parse(sessionStorage.getItem(DRAFT) || "null"); } catch { return null; } };
  const saveDraft = () => { try { const d = { what: F.what.value, done: F.done.value, name: F.name.value, model: F.model.value, split: F.split.checked, ro: F.ro.checked }; if (d.what.trim() || d.done.trim()) sessionStorage.setItem(DRAFT, JSON.stringify(d)); else sessionStorage.removeItem(DRAFT); } catch { /* a private window: the draft lives only here */ } };
  const dropDraft = () => { try { sessionStorage.removeItem(DRAFT); } catch { /* nothing kept */ } };

  function changePage() {
    if (!F.what) {
      F.what = h("textarea", { class: "field", rows: "4", placeholder: "What should change? In your words: Customers get free shipping over $50.", "aria-label": "What should change" });
      F.done = h("input", { class: "field", placeholder: "Done when: `python3 -m pytest -q` passes, and the cart shows free shipping over $50", "aria-label": "Done when" });
      F.name = h("input", { class: "field", placeholder: "picked for you", maxlength: "32", "aria-label": "Name", spellcheck: "false" });
      F.model = h("select", { class: "field", "aria-label": "Model" }, ["", "haiku", "sonnet", "opus"].map((m) => h("option", { value: m }, m || "Claude Code's default")));
      F.edits = h("input", { type: "checkbox", checked: true });
      F.deny = h("input", { type: "checkbox", checked: true });
      F.ro = h("input", { type: "checkbox" });
      F.extra = h("input", { class: "field", placeholder: "Bash(python3 serve.py:*)", "aria-label": "Also allowed without asking", spellcheck: "false" });
      F.split = h("input", { type: "checkbox" });
      F.err = h("div", { class: "g-err", role: "alert", hidden: true });
      F.chips = h("div", { class: "chips" });
      F.doneSay = h("p", { class: "g-dim g-donesay" });
      F.flakyHost = h("div");
      F.intent = h("div", { class: "g-intent", hidden: true });
      F.routeHost = h("p", { class: "g-dim g-route" }, "The model: Workbench suggests one from this repository's past tasks once you say what should change.");
      F.guardHost = h("div", { class: "g-guard" }, h("p", { class: "g-dim" }, "Reading the guarded paths…"));
      F.go = btn("Start agent", { primary: true, type: "submit" });
      const d = readDraft();
      if (d) { F.what.value = d.what || ""; F.done.value = d.done || ""; F.name.value = d.name || ""; F.model.value = d.model || ""; F.split.checked = !!d.split; F.ro.checked = !!d.ro; }
      F.check = () => {
        const t = `${F.what.value} ${F.done.value}`;
        const hasDone = /`[^`]+`/.test(F.done.value) || /\bpasses?\b|\bshows?\b/.test(F.done.value);
        const others = ctx.state.agents.filter((a) => a.repo === p.path && a.running && a.state === "working");
        F.chips.replaceChildren(
          h("span", { class: `chip${hasDone ? " add" : ""}` }, hasDone ? "has a done check" : "no done check yet"),
          ...(S.rules || []).filter((r) => r.text && t.trim() && r.text.split(/\W+/).some((w) => w.length > 4 && t.toLowerCase().includes(w.toLowerCase()))).slice(0, 2).map((r) => h("span", { class: "chip", title: r.text }, "◆ rule applies")),
          ...others.slice(0, 2).map((a) => h("span", { class: "chip del" }, `${a.name} is working here now`)));
        F.doneSay.textContent = doneSentence(F.done.value);
        const fl = flakyFor(F.done.value, S.flaky || []);
        F.flakyHost.replaceChildren(...[fl.length ? h("div", { class: "callout warn g-flaky" }, icon("warn", "i"), h("div", {},
          h("b", {}, fl.length === 1 ? `${fl[0].test} is flaky` : `${fl.length} tests this check runs are flaky`),
          h("p", {}, `${fl.slice(0, 3).map((x) => `${x.test}${x.flaky_why ? ` (${x.flaky_why})` : ""}`).join("; ")}. It passed and failed in recent runs without a change, so this done check can fail for no fault of the agent's. Name a test that is steady, or say that it is to run twice.`),
          h("a", { class: "lv-link", href: "#/debug/failing" }, "See the runs"))) : null].filter(Boolean));
        F.go.replaceChildren(S.sending ? "Starting…" : F.split.checked ? "Plan the parts" : F.ro.checked ? "Start, read only" : "Start agent");
        F.edits.disabled = F.ro.checked;
      };
      F.paintGuard = () => {
        const g = S.guarded;
        if (!g) return;
        const paths = g.filter((x) => x.kind === "path"), other = g.filter((x) => x.kind !== "path");
        F.guardHost.replaceChildren(...[
          paths.length ? h("p", {}, h("b", {}, "Guarded paths: "), "it may read these, not change them: ", ...paths.flatMap((x, i) => [i ? ", " : "", h("code", {}, x.label)]), ".") : h("p", { class: "g-dim" }, "No guarded paths in this project: it may change any file on its branch."),
          other.length ? h("p", { class: "g-dim" }, "Denied, and sent to you as a request: ", other.map((x) => x.label).join("; "), ".") : null,
          h("a", { class: "lv-link", href: "#/more/rules" }, "Change them in More › Rules")].filter(Boolean));
      };
      let routeT = 0, routeN = 0;
      const route = () => {
        clearTimeout(routeT);
        routeT = setTimeout(async () => {
          const task = F.what.value.trim();
          if (task.split(/\s+/).length < 3) return;
          const mine = ++routeN;
          try {
            const r = await ctx.api(`/api/route?repo=${enc(p.path)}&task=${enc(task)}`);
            if (mine !== routeN || !alive) return;
            const sug = r && r.suggestion && r.apply ? r.suggestion.model : null;
            F.routeHost.replaceChildren(...(sug
              ? [h("b", {}, `Suggested: ${sug === "default" ? "Claude Code's default" : sug}. `), r.why || "", " ", F.model.value === (sug === "default" ? "" : sug) ? h("span", { class: "chip add" }, "chosen") : h("button", { class: "lv-link", type: "button", onclick: () => { F.model.value = sug === "default" ? "" : sug; saveDraft(); route(); } }, "Use it")]
              : [h("b", {}, "Not enough history yet. "), (r && r.why) || "No past task here is like this one: the model is your choice."]));
          } catch { /* the hint is a nicety */ }
        }, 500);
      };
      const changed = () => { F.intent.hidden = true; F.ok = null; F.check(); saveDraft(); };
      for (const f of [F.what, F.done, F.name, F.extra]) f.addEventListener("input", changed);
      for (const f of [F.model, F.split, F.ro, F.edits, F.deny]) f.addEventListener("change", changed);
      F.what.addEventListener("input", route);
      import("/levels/complete.js").then((m) => { if (alive) m.attachComplete(F.what, { field: "task", ctx, repo: () => p.path }); }).catch(() => {});
      F.check();
      F.paintGuard();
      if (F.what.value) route();
      if (ctx.params.query && ctx.params.query.get("focus") !== "0") queueMicrotask(() => F.what.focus());
    }
    // ⌘↵ (Ctrl↵ off a Mac) from any field of the form starts it.
    const form = h("form", { class: "g-comp pane", onsubmit: (e) => { e.preventDefault(); start(); }, onkeydown: (e) => { if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); e.stopPropagation(); start(); } } },
      h("div", { class: "bd" },
        h("label", { class: "g-l" }, "What should change"), F.what,
        h("label", { class: "g-l" }, "Done when"), F.done, F.doneSay, F.chips, F.flakyHost,
        h("div", { class: "g-row2" },
          h("div", {}, h("label", { class: "g-l" }, "Model"), F.model),
          h("div", {}, h("label", { class: "g-l" }, "Name"), F.name)),
        F.routeHost,
        h("label", { class: "g-tick" }, F.split, h("span", {}, h("b", {}, "Split it into parts"), " A lead drafts the parts first; nothing starts until you approve the plan.")),
        h("fieldset", { class: "g-perm" }, h("legend", {}, label("May do without asking")),
          h("label", { class: "g-tick" }, F.ro, h("span", {}, h("b", {}, "Read only"), " It reads the code and answers; it changes no file and runs nothing that writes.")),
          h("label", { class: "g-tick" }, F.edits, h("span", {}, h("b", {}, "Edit files on its branch and run the tests"), " Reading commands too (ls, grep, git log); it asks for anything else, and for anything on the network.")),
          h("label", { class: "g-tick" }, F.deny, h("span", {}, h("b", {}, "Never"), " ", h("code", {}, "git push"), ", ", h("code", {}, "rm -rf"), ", ", h("code", {}, "pip install"))),
          F.guardHost,
          h("label", { class: "g-l" }, "Also allowed (Claude Code rules, comma separated)"), F.extra),
        F.intent, F.err),
      h("div", { class: "g-foot" }, h("span", { class: "g-dim" }, kc("⌘↵"), " from any field"), h("span", { class: "sp" }), F.go));
    return page({ kicker: p.project, title: "Start a change", sub: "Say what should change and how you will know it is done.", inspector: changeInsp(), wide: true },
      projectSteps(p, "change"), form);
  }

  /** Start a change's inspector: drawn again on its own, so the composer keeps what you typed. */
  function changeInsp() {
    const list = fleet(p.path).filter((a) => a.running);
    return [
      h("section", {}, label("Work in progress", h("span", { class: "num" }, String(list.length))),
        list.length ? h("div", { class: "pane" }, list.map((a) => agentRow(a, groupOf(a), true))) : h("p", { class: "g-dim" }, "No agent is working in this project.")),
      h("section", {}, label("What it starts with"),
        h("dl", { class: "kv" }, h("dt", {}, "Memory"), h("dd", {}, "Remembers this project's decisions (ChloeCtx), in its own session's settings; yours are not touched."),
          h("dt", {}, "Branch"), h("dd", {}, "Its own worktree on wb/<name> from the project's HEAD. Main is never edited."),
          h("dt", {}, "Told"), h("dd", {}, "Your words, the done check, and the project's rules."))),
    ];
  }

  /** The intent check, before anything starts: the task's sentences
   *  against the project's rules, its guarded paths and the verbs that
   *  destroy (no model). You confirm or edit. */
  function showIntent(found) {
    const what = F.what.value;
    F.intent.hidden = false;
    F.intent.replaceChildren(h("div", { class: "callout warn" }, icon("warn", "i"), h("div", {},
      h("b", {}, found.length === 1 ? "One sentence asks for something this project guards" : `${found.length} sentences ask for something this project guards`),
      h("ul", { class: "g-ilist" }, found.map((f) => h("li", {}, h("q", {}, f.sentence), " ", h("span", { class: "g-dim" }, f.why)))),
      h("div", { class: "g-irow" },
        btn("Remove those sentences", { sm: true, onclick: () => { let t = what; for (const f of found) t = t.replace(f.sentence, ""); F.what.value = t.replace(/[ \t]{2,}/g, " ").replace(/\n{3,}/g, "\n\n").trim(); F.intent.hidden = true; F.check(); saveDraft(); F.what.focus(); } }),
        btn("Start read only", { sm: true, onclick: () => { F.ro.checked = true; F.ok = F.what.value; F.check(); start(); } }),
        btn("Start anyway", { sm: true, onclick: () => { F.ok = F.what.value; start(); } }),
        btn("Edit", { sm: true, ghost: true, onclick: () => { F.intent.hidden = true; F.what.focus(); } })))));
    const first = F.intent.querySelector(".btn");
    if (first) first.focus();
  }

  async function start() {
    const what = F.what.value.trim(), done = F.done.value.trim();
    F.err.hidden = true;
    if (!what) { F.err.textContent = "Say what should change first."; F.err.hidden = false; F.what.focus(); return; }
    if (S.sending) return;
    if (F.ok !== F.what.value) {
      const found = intentCheck(what, { rules: S.rules || [], guarded: S.guarded || [], readOnly: F.ro.checked });
      if (found.length) { showIntent(found); return; }
    }
    F.intent.hidden = true;
    const task = joinTask(what, done);
    S.sending = true;
    F.go.disabled = true; F.check();
    try {
      if (F.split.checked) {
        planFrom({ repo: p.path, idea: what, done, model: F.model.value });
        dropDraft();
        ctx.go(`#/new/plan?repo=${enc(p.path)}`);
        return;
      }
      const allow = F.extra.value.split(",").map((s) => s.trim()).filter(Boolean);
      const deny = F.deny.checked ? ["Bash(git push:*)", "Bash(rm -rf:*)", "Bash(pip install:*)"] : [];
      const body = { repo: p.path, task, worktree: true, permissions: { allow, deny } };
      if (F.ro.checked) body.read_only = true;
      else if (!F.edits.checked) body.ask_edits = true;
      if (F.name.value.trim()) body.name = F.name.value.trim();
      if (F.model.value) body.model = F.model.value;
      const { agent } = await ctx.api("/api/agents", { method: "POST", body });
      ctx.toast(`Started ${agent.name}${agent.branch ? ` on ${agent.branch}` : ""}${F.ro.checked ? ", read only" : ""}`);
      dropDraft();
      F.what = null;
      ctx.go(`#/a/${agent.id}`);
    } catch (e) {
      F.err.textContent = e.status === 429 ? `${e.message}. Stop one first.` : e.message;
      F.err.hidden = false;
    } finally { S.sending = false; if (alive && F.what) { F.go.disabled = false; F.check(); } }
  }

  function draw() {
    if (!alive) return;
    const fresh = ctx.projectByName(p.project) || p;
    Object.assign(p, fresh);
    // Start a change is drawn once: its fields are kept, the inspector follows.
    if (step === "change" && el.querySelector(".g-comp")) { refreshInsp(); return; }
    // The ask box is moved, not rebuilt: give it its focus and caret back.
    const f = document.activeElement;
    const keep = f && el.contains(f) && f.matches("input,textarea") ? [f, f.selectionStart, f.selectionEnd] : null;
    el.replaceChildren(step === "understand" ? understandPage() : step === "change" ? changePage() : continuePage());
    if (keep && keep[0].isConnected) { keep[0].focus({ preventScroll: true }); try { keep[0].setSelectionRange(keep[1], keep[2]); } catch { /* not a text field */ } }
  }
  function refreshInsp() {
    const old = el.querySelector(".gp-insp");
    if (old) old.replaceChildren(...changeInsp());
    if (F.what) { F.check(); F.paintGuard(); }
  }

  load();
  draw();
  return {
    update: draw,
    reload: () => load(), // after an answer: "Since you left" reads the digest again
    focusComposer: () => { if (F.what) F.what.focus(); },
    unmount: () => { alive = false; for (const v of S.views) try { v.unmount(); } catch {} el.remove(); },
  };
}

function openList(ctx) {
  const ps = ctx.projects();
  if (!ps.length) return null;
  return pane("Projects Workbench knows", null, h("table", { class: "tbl g-ptbl" }, h("tbody", {}, ps.map((p) => h("tr", { "data-href": `#/p/${p.project}`, tabindex: "0", "data-nav": "", onclick: () => ctx.go(`#/p/${p.project}`) }, h("td", {}, h("b", {}, p.project)), h("td", { class: "g-dim g-ppath" }, h("code", {}, p.path)), h("td", { class: "r" }, btn("Continue", { sm: true, href: `#/p/${p.project}` })))))));
}

/** After a folder is opened: its project's page, found by its path. */
export function goProject(ctx, repo) {
  const path = typeof repo === "string" ? repo : repo && repo.path;
  const proj = (repo && repo.project && ctx.projectByName(repo.project)) || (ctx.projectOf && ctx.projectOf(path)) || ctx.projects().find((x) => x.path === path);
  ctx.go(proj ? `#/p/${proj.project}` : "#/p");
}

/** The task as the agent reads it: your words, a full stop, then the done
 *  check as a sentence ("Done when: … passes."). */
export function joinTask(what, done) {
  const stop = (s) => (/[.!?:)`"”]$/.test(s) ? s : `${s}.`);
  const w = what.trim(), d = done.trim().replace(/^done when:?\s*/i, "");
  return d ? `${stop(w)}\n\nDone when: ${stop(d)}` : w;
}

/** How the done check will be checked, in a sentence. */
export function doneSentence(done) {
  const d = String(done || "").trim();
  if (!d) return "No done check yet: the agent stops when it thinks it is done. A command in backticks, such as `python3 -m pytest -q`, makes it check.";
  const cmd = (d.match(/`([^`]+)`/) || [])[1];
  if (cmd) return `How it is checked: the agent is told to run ${cmd} in its worktree and to stop only when it passes; each run is recorded, so you see the last result before you merge.`;
  return "How it is checked: no command, so the agent judges this sentence itself, and you check it in Review. Put a command in backticks to have it run.";
}

/** The flaky tests a done check would run: those it names, or all of
 *  them when it runs a whole suite. */
export function flakyFor(done, flaky) {
  const d = String(done || "");
  if (!d.trim() || !flaky.length) return [];
  const cmd = ((d.match(/`([^`]+)`/) || [])[1] || d).trim();
  const named = flaky.filter((t) => { const leaf = String(t.test).split(/::|\.|\/| /).filter(Boolean).pop() || t.test; return leaf.length > 2 && (d.includes(t.test) || d.includes(leaf)); });
  if (named.length) return named;
  const suite = /\b(pytest|cargo test|npm (run )?test|go test|jest|vitest|mvn test|make test|rspec|unittest|node --test)\b/.test(cmd);
  // A suite with no test named (flags only) runs every test.
  const narrowed = suite && cmd.replace(/^.*?\b(pytest|cargo test|npm (run )?test|go test|jest|vitest|mvn test|make test|rspec|unittest|node --test)\b/, "").split(/\s+/).some((w) => w && !w.startsWith("-"));
  return suite && !narrowed ? flaky : [];
}

const STOP = new Set("the a an and or of to in on for with that this it its is are be by from as at not no never dont don't do does must should any all every always only into than then them they their there these those when what which while will would can could".split(" "));
const sig = (s) => String(s).toLowerCase().replace(/[’']/g, "").split(/[^a-z0-9_./-]+/).filter((w) => w.length > 3 && !STOP.has(w));
const DESTROY = [
  [/\brm\s+-(rf|fr|r)\b/i, "deletes files with rm"],
  [/\b(delete|remove|erase)\s+(all|every|the whole|everything)\b/i, "deletes"],
  [/\b(delete|remove)\b[^.]*\b(folder|directory|directories|branch|branches|table|tables|database|data|history|migrations?|files)\b/i, "deletes"],
  [/\bdrop\s+(the\s+)?(table|database|column|schema)s?\b/i, "drops data"],
  [/\bforce[- ]push|\bpush\s+(--force|-f)\b/i, "force-pushes"],
  [/\bgit\s+push\b|\bpush\s+(it\s+)?(to\s+)?(main|master|origin)\b/i, "pushes"],
  [/\breset\s+--hard\b/i, "throws work away with git reset --hard"],
  [/\b(wipe|truncate|purge)\b/i, "wipes data"],
  [/\brewrite\s+(the\s+)?(git\s+)?history\b/i, "rewrites history"],
  [/\b(pip|npm|cargo|brew|apt)\s+(install|add)\b/i, "installs packages"],
];

/** The intent check (no model): each sentence of the task against the
 *  project's rules, its guarded paths and the verbs that destroy. A
 *  sentence that says not to do something is a constraint, and passes. */
export function intentCheck(text, { rules = [], guarded = [], readOnly = false } = {}) {
  const out = [];
  const sentences = String(text).split(/(?<=[.!?])\s+|\n+/).map((s) => s.trim()).filter(Boolean);
  const neg = /^(do not|don['’]t|never|without|avoid|no\b|keep|leave)\b/i;
  const negRules = rules.filter((r) => r.text && /\b(never|not|don['’]?t|no)\b/i.test(r.text));
  const globRe = (g) => new RegExp(`(^|[\\s\`'"(/])${g.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*\*\/?/g, "\u0001").replace(/\*/g, "[^\\s/]*").replace(/\u0001/g, ".*")}`, "i");
  for (const s of sentences) {
    if (neg.test(s)) continue;
    const why = [];
    if (!readOnly) { const did = DESTROY.filter(([re]) => re.test(s)).map(([, w]) => w); if (did.length) why.push(`it ${[...new Set(did)].join(" and ")}`); }
    for (const g of guarded) {
      if (g.kind === "path" && g.label) {
        const bare = g.label.replace(/\/?\*+$/, "").replace(/^\*+\/?/, "");
        if ((bare && bare.length > 2 && s.includes(bare)) || globRe(g.label).test(s)) why.push(`it names ${g.label}, a guarded path agents may not change`);
      } else if (g.kind === "prefix" && g.label && s.toLowerCase().includes(g.label.toLowerCase())) why.push(`it runs ${g.label}, which always asks first`);
    }
    const words = new Set(sig(s));
    for (const r of negRules) {
      const shared = [...new Set(sig(r.text))].filter((w) => words.has(w));
      if (shared.length >= 2 || (shared.length === 1 && shared[0].length > 6 && sig(r.text).length <= 4)) { why.push(`it may go against the rule “${r.text}”`); break; }
    }
    if (why.length) out.push({ sentence: s, why: [...new Set(why)].join("; ") });
  }
  return out;
}

function picker(ctx) {
  let trust = true;
  return h("div", {},
    // onDone gets the folder's path: land on its project, never `#/p/`.
    repoPicker(ctx, { title: "Open a folder", known: false, focus: !ctx.projects().length, body: () => ({ trust }), onDone: async (repo) => { await ctx.refreshProjects(); goProject(ctx, repo); } }),
    h("label", { class: "g-tick" }, h("input", { type: "checkbox", checked: true, onchange: (e) => { trust = e.target.checked; } }), h("span", {}, h("b", {}, "Let agents work here"), " Workbench answers Claude Code's “trust this folder?” for this project's folders.")));
}

const CSS = `
.g-ptbl .g-ppath{white-space:normal;width:100%}.g-ptbl .g-ppath code{overflow-wrap:anywhere;word-break:break-all}
@media (max-width:640px){.g-ptbl td.r{display:none}}
.g-psteps{display:flex;gap:var(--s-5);border-bottom:var(--hair) solid var(--line);margin:0 0 var(--s-5);overflow-x:auto;align-items:center}
.g-psteps a{padding:10px 0;font-size:var(--fs-body);color:var(--text-2);margin-bottom:-1px;border-bottom:1.5px solid transparent;text-decoration:none;white-space:nowrap;display:inline-flex;align-items:center;gap:4px}
.g-psteps a.on{color:var(--text);font-weight:var(--w-mid);border-bottom-color:var(--text)}
.g-psteps .sp{flex:1}
.g-psteps .g-ship{color:var(--accent-text)}
.g-ask{display:flex;align-items:center;gap:var(--s-2);padding:0 var(--s-2) 0 var(--s-3);border:var(--hair) solid var(--line-2);border-radius:var(--r-sm);background:var(--surface)}
.g-ask .field{border:0;box-shadow:none;padding:0;background:transparent}
.g-ask:focus-within{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-soft)}
.g-ask svg{color:var(--text-2)}
.g-ask.big{height:44px}
.g-needs{display:flex;flex-direction:column;gap:var(--s-3);margin-top:var(--s-4)}
.g-arow{display:grid;grid-template-columns:20px minmax(90px,auto) auto minmax(0,1fr) auto auto;gap:var(--s-3);align-items:center;padding:10px var(--s-4);border-bottom:var(--hair) solid var(--line);min-height:44px}
.g-arow:last-child{border-bottom:0}
.g-arow .nm{font-weight:var(--w-mid);color:var(--text);text-decoration:none}
.g-arow .live{color:var(--text-2);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.g-arow .acts{display:flex;gap:var(--s-2)}
.g-arow.compact{padding:4px var(--s-3);min-height:32px}
.g-arow:focus-visible{outline:2px solid var(--accent);outline-offset:-2px}
.gp-insp .g-arow{grid-template-columns:20px minmax(0,1fr) auto;padding:8px var(--s-3)}.gp-insp .g-arow .acts,.gp-insp .g-arow .churn,.gp-insp .g-arow>span:empty{display:none}.gp-insp .g-arow .live{grid-column:2/-1;font-size:var(--fs-sm)}
.g-fold summary{cursor:pointer;color:var(--text-2);font-size:var(--fs-sm);padding:var(--s-1) 0 var(--s-2)}
.g-chg{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:2px var(--s-2);padding:var(--s-2) 0;border-bottom:var(--hair) solid var(--line);color:var(--text);text-decoration:none}
.g-chg code{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.g-chg .churn{grid-column:1/-1}
.g-dec{display:grid;grid-template-columns:12px minmax(0,1fr);gap:var(--s-2);padding:var(--s-2) 0;border-bottom:var(--hair) solid var(--line)}
.g-dec small{display:block;color:var(--text-2);font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);margin-top:2px}
.g-dim{color:var(--text-2);margin:0}
.g-bad{color:var(--crit-ink)}
.g-md p{margin:0 0 var(--s-2)}
.g-empty{padding:var(--s-5);display:flex;flex-direction:column;gap:var(--s-2);align-items:flex-start}
.g-empty p{margin:0;color:var(--text-2)}
.g-map{position:relative;height:min(62vh,560px);border:var(--hair) solid var(--line);border-radius:var(--r);overflow:hidden;background:var(--surface)}
.g-map .lv-screen{position:absolute;inset:0;overflow:auto}
.g-rule{padding:var(--s-2) 0;border-bottom:var(--hair) solid var(--line)}
.g-comp .bd{display:flex;flex-direction:column;gap:var(--s-2)}
.g-l{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2);margin-top:var(--s-2)}
.g-row2{display:grid;grid-template-columns:1fr 1fr;gap:var(--s-3)}
.g-row2>div{display:flex;flex-direction:column;gap:var(--s-2)}
.g-tick{display:flex;gap:var(--s-2);align-items:flex-start;padding:var(--s-2) 0;color:var(--text-2);font-size:var(--fs-sm)}
.g-tick b{color:var(--text);font-weight:var(--w-mid)}
.g-tick input{margin-top:3px;accent-color:var(--accent)}
.g-perm{border:0;border-top:var(--hair) solid var(--line);margin:var(--s-2) 0 0;padding:var(--s-3) 0 0;display:flex;flex-direction:column;gap:var(--s-1)}
.g-perm legend{padding:0 0 var(--s-1)}
.g-err{padding:var(--s-2) var(--s-3);border-radius:var(--r-sm);background:var(--crit-soft);color:var(--crit-ink)}
.g-foot{display:flex;align-items:center;gap:var(--s-3);padding:var(--s-3) var(--s-4);border-top:var(--hair) solid var(--line)}
.g-foot .sp{flex:1}
.chips{display:flex;gap:6px;flex-wrap:wrap}
.g-askw{display:flex;flex-direction:column;gap:var(--s-3)}
.g-ask .btn .spin{width:11px;height:11px;margin-right:6px}
.g-ans{min-height:0}
.g-ans[hidden]{display:none}
.g-asking{display:flex;align-items:center;gap:var(--s-2)}
.g-ansv{display:flex;flex-direction:column;gap:var(--s-3);padding:var(--s-4);border:var(--hair) solid var(--line);border-radius:var(--r);background:var(--surface)}
.g-asum{margin:0;font-size:var(--fs-body);line-height:var(--lh-body);color:var(--text)}
.g-acard{display:flex;flex-direction:column;gap:var(--s-2)}
.g-arec,.g-acode,.g-acommit{padding:var(--s-2) 0;border-top:var(--hair) solid var(--line);min-width:0}
.g-ameta{display:flex;flex-wrap:wrap;gap:4px var(--s-2);align-items:baseline;font-size:var(--fs-sm);color:var(--text-2)}
.g-ameta b{color:var(--text);font-weight:var(--w-mid)}
.g-ameta code{overflow-wrap:anywhere}
.g-asrc{margin-left:auto;font-size:var(--fs-sm);color:var(--accent-text);white-space:nowrap}
.g-atext{margin:4px 0 0;color:var(--text);overflow-wrap:anywhere}
.g-atext.q{color:var(--text-2);font-style:italic}
.g-asnip{margin:6px 0 0;padding:var(--s-2) var(--s-3);background:var(--surface-2);border-radius:var(--r-sm);font:var(--fs-xs)/1.5 var(--mono);overflow-x:auto;white-space:pre;max-height:10em}
.g-acommit{display:grid;grid-template-columns:auto minmax(0,1fr) auto;gap:var(--s-2);align-items:baseline;font-size:var(--fs-sm)}
.g-acommit span:nth-child(2){overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.g-donesay{font-size:var(--fs-sm);line-height:var(--lh-sm)}
.g-route{font-size:var(--fs-sm);line-height:var(--lh-sm)}
.g-route b{color:var(--text);font-weight:var(--w-mid)}
.g-guard{display:flex;flex-direction:column;gap:2px;padding:var(--s-2) 0;font-size:var(--fs-sm);color:var(--text-2)}
.g-guard p{margin:0}.g-guard b{color:var(--text);font-weight:var(--w-mid)}
.g-guard code{font-size:var(--fs-xs)}
.g-intent[hidden]{display:none}
.g-ilist{margin:var(--s-2) 0;padding-left:18px}
.g-ilist li{margin:2px 0}
.g-ilist q{color:var(--text)}
.g-irow{display:flex;flex-wrap:wrap;gap:var(--s-2);margin-top:var(--s-2)}
.g-flaky .lv-link{font-size:var(--fs-sm)}
.g-pmodel{appearance:auto;cursor:pointer}
@media (max-width:640px){.g-acommit{grid-template-columns:auto minmax(0,1fr)}.g-acommit span:last-child{grid-column:1/-1}.g-asrc{margin-left:0}}
@media (max-width:640px){
  .g-arow{grid-template-columns:20px minmax(0,1fr) auto;row-gap:2px}
  .g-arow .live{grid-column:2/-1}
  .g-arow .churn{grid-column:2/-1}
  .g-arow .acts{grid-column:1/-1}
  .g-arow .acts .btn{flex:1;height:var(--touch)}
  .g-row2{grid-template-columns:1fr}
  .g-map{height:360px}
  .g-foot .btn{flex:1;height:var(--touch)}
  .g-foot .g-dim{display:none}
  .g-proj .gp{display:flex;flex-direction:column}
}
`;
