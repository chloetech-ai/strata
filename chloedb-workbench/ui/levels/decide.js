// Decide (SPEC.md, "Levels", Plan for the fleet): the agents' open
// questions, each with what ChloeCtx found when it was asked (the option
// that matches a past decision marked, the decision itself with its Rewind),
// answered in one click and, if you like, made a rule every agent then gets.
// A permission prompt shows the whole command (or the edit's diff) and the
// prompt's own options, as its agent's screen lists them (SPEC.md, "Sixth
// build: Safe answering"); Workbench types one only while that prompt is on
// the screen. Below: what rules answered for you today, and the rules,
// each one on or off.
import {
  h, useStyle, toMs, fmtDur, fmtClock, avatar, pill, repoOf, mergeAgents, inlineCode, parseJSON, fmtDay,
  keyNav, rerender, routeParam, precedentOf,
} from "./common.js";

const enc = encodeURIComponent;
const names = (xs) => (xs.length < 2 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs.at(-1)}`);
const plural = (n, one, many = one + "s") => `${n} ${n === 1 ? one : many}`;
const isToday = (t) => { const d = new Date(toMs(t) || 0); return d.toDateString() === new Date().toDateString(); };
// A permission card's title, from the tool call it asks about.
export function permTitle(q, name) {
  const d = q.detail || {};
  const m = d.menu || {};
  const file = d.rel || (d.file ? String(d.file).split("/").pop() : "");
  if (m.kind === "trust") return `${name} asks: trust this folder?`;
  if (d.action) return `${name} wants to ${d.action.charAt(0).toLowerCase()}${d.action.slice(1)}`;
  if (d.tool === "Bash") return `${name} wants to run a command`;
  if (d.tool === "Write") return `${name} wants to write ${file}`;
  if (d.tool && /Edit$/.test(d.tool)) return `${name} wants to edit ${file}`;
  if (d.tool) return `${name} wants to use ${d.tool.replace(/^mcp__(\w+?)__/, "$1's ")}`;
  return m.question || q.text || `${name} waits on a choice`;
}
// Change requests (guarded actions): a section of its own (ui/levels/change-requests.js).
import { changeRequests } from "./change-requests.js";
// Sixth build: the Rules panel, with add, edit and delete (ui/levels/rules.js).
import { rulesPanel } from "./rules.js";

export function mount(root, ctx) {
  let wanted = routeParam(ctx, "question");
  try { wanted = wanted || sessionStorage.getItem("wb.decide.q"); sessionStorage.removeItem("wb.decide.q"); } catch {}
  const S = {
    repo: repoOf(ctx), open: [], answered: [], rules: [], outcomes: [], stream: (ctx.state && ctx.state.agents) || [],
    sel: wanted || null, loaded: false, error: null, choice: new Map(), ruleOn: new Map(), ruleText: new Map(), own: new Map(),
    sending: null, done: null, review: false, fresh: new Set(), toggling: new Set(),
    seq: (((ctx.state && ctx.state.events) || []).at(-1) || {}).seq || 0,
  };
  const el = h("div", { class: "lv-screen de" });
  root.append(el);
  const render = () => rerender(el, view);
  let alive = true;
  const rulesP = rulesPanel(ctx, {
    repo: () => S.repo, rules: () => S.rules, setRules: (l) => { S.rules = l; }, render: () => { if (alive) render(); },
    toggle: (r) => toggleRule(r), fresh: S.fresh, questionText: (id) => [...S.open, ...S.answered].find((x) => x.id === id)?.text,
  });

  // ------------------------------------------------------------- data
  async function load() {
    const q = S.repo ? `repo=${enc(S.repo)}&` : "";
    try {
      const [o, a, r] = await Promise.all([
        ctx.api(`/api/questions?${q}state=open`),
        ctx.api(`/api/questions?${q}state=answered`).catch(() => ({ questions: [] })),
        ctx.api(`/api/rules${S.repo ? `?repo=${enc(S.repo)}` : ""}`).catch(() => ({ rules: [] })),
      ]);
      S.open = (o.questions || []).sort((x, y) => toMs(x.created) - toMs(y.created));
      S.answered = (a.questions || []).filter((x) => isToday(x.answered || x.created)).sort((x, y) => toMs(y.answered) - toMs(x.answered));
      S.rules = (r.rules || []).sort((x, y) => toMs(y.created) - toMs(x.created));
      if (!S.sel || ![...S.open, ...S.answered].some((x) => x.id === S.sel)) S.sel = S.open[0]?.id || null;
      S.loaded = true; S.error = null;
      if (alive) render();
      loadOutcomes();
      // ChloeCtx answers about a new question a moment after it is asked.
      clearTimeout(S.again);
      if (alive && S.open.some((x) => x.kind !== "permission" && precedentOf(x).pending) && (S.tries = (S.tries || 0) + 1) <= 15) S.again = setTimeout(load, 2000);
      else S.tries = 0;
    } catch (e) {
      S.error = e.message || String(e); S.loaded = true;
      if (alive) render();
    }
  }

  // The goal's outcomes, to say what a question blocks.
  async function loadOutcomes() {
    try {
      const { goals } = await ctx.api(`/api/goals${S.repo ? `?repo=${enc(S.repo)}` : ""}`);
      const g = goals.find((x) => x.state === "active");
      if (!g) return;
      const d = await ctx.api(`/api/goals/${enc(g.id)}`);
      S.outcomes = d.outcomes || [];
      if (alive) render();
    } catch {}
  }

  const agents = () => mergeAgents(S.stream);
  const agentOf = (id) => agents().get(id) || null;
  const opts = (q) => parseJSON(q.options, []) || [];
  const prec = (q) => precedentOf(q);
  const matchIdx = (q) => opts(q).findIndex((o) => o.matches_precedent);
  const defaultChoice = (q) => (q.kind === "permission" ? 0 : matchIdx(q) >= 0 ? matchIdx(q) : q.recommended ?? 0);
  const choiceOf = (q) => (S.choice.has(q.id) ? S.choice.get(q.id) : defaultChoice(q));
  const ruleFor = (q, i) => {
    if (S.own.has(q.id)) return S.ruleText.get(q.id);
    const o = opts(q)[i];
    if (q.suggested_rule && i === defaultChoice(q)) return q.suggested_rule;
    return o ? `${q.text.replace(/\?\s*$/, "")}: ${o.label.replace(/\.$/, "")}.` : "";
  };

  function blocked(q) {
    if (!q.outcome) return [];
    const chain = [], seen = new Set([q.outcome]);
    const first = S.outcomes.find((o) => o.id === q.outcome);
    if (first) chain.push(first);
    let grew = true;
    while (grew) { grew = false; for (const o of S.outcomes) if (o.after && seen.has(o.after) && !seen.has(o.id)) { seen.add(o.id); chain.push(o); grew = true; } }
    return chain;
  }

  // ------------------------------------------------------------- actions
  async function answer(q, body, said) {
    S.sending = q.id; S.done = q.kind === "permission" ? { id: q.id, ok: true, text: `Choosing “${said}” in ${agentOf(q.agent)?.name || "its"} prompt…` } : S.done; render();
    try {
      const r = await ctx.api(`/api/questions/${enc(q.id)}/answer`, { method: "POST", body });
      const a = agentOf(q.agent);
      S.done = { id: q.id, ok: true, text: `Answered “${said}”.`, more: [a ? `${a.name} goes on` : null, r && r.rule ? "the rule is saved; every agent gets it at its next prompt" : null].filter(Boolean).join("; ") };
      if (r && r.rule) S.fresh.add(r.rule.id);
      const next = S.open.find((x) => x.id !== q.id);
      S.sel = next ? next.id : q.id;
      await load();
    } catch (e) {
      S.done = { id: q.id, ok: false, text: `Not answered. ${e.message}` };
      // A prompt that left the screen closed its question: show what is open.
      if (e.status === 409) await load();
    }
    S.sending = null;
    if (alive) render();
  }

  async function toggleRule(r) {
    if (S.toggling.has(r.id)) return;
    S.toggling.add(r.id);
    const was = r.active;
    r.active = !was; render();
    try { const out = await ctx.api(`/api/rules/${enc(r.id)}`, { method: "PATCH", body: { active: !was } }); if (out && out.rule) Object.assign(r, out.rule); }
    catch (e) { r.active = was; S.done = { ok: false, text: `The rule was not changed: ${e.message}` }; }
    S.toggling.delete(r.id);
    if (alive) render();
  }

  // ------------------------------------------------------------- view
  function view() {
    const head = h("header", { class: "de-head" },
      h("div", {}, h("h1", {}, "Decisions"), h("div", { class: "muted" }, "Agents don't stop and wait in a terminal. Their questions come here, with what you decided before.")),
      S.loaded ? h("div", { class: "de-tally" },
        h("span", {}, h("b", {}, S.open.length), " open"),
        h("span", {}, h("b", {}, S.answered.filter((q) => String(q.answered_by || "").startsWith("rule:")).length), " answered by rules today"),
        h("span", {}, h("b", {}, S.rules.filter((r) => r.active).length), " rules on")) : null);
    if (!S.loaded) return h("div", { class: "de-wrap" }, head, h("div", { class: "de-skel" }));
    if (S.error) return h("div", { class: "de-wrap" }, head, h("div", { class: "lv-card de-err" }, h("b", {}, "Decide could not load."), h("div", { class: "muted" }, S.error), h("button", { class: "btn sm", "data-key": "retry", onclick: load }, "Try again")));
    const q = [...S.open, ...S.answered].find((x) => x.id === S.sel) || null;
    return h("div", { class: "de-wrap" }, head, changeRequests(ctx),
      h("div", { class: "de-grid" },
        h("div", { class: "de-left" }, openList(), answeredCard()),
        h("div", { class: "de-right" }, banner(), q ? (q.state === "open" ? detail(q) : answeredDetail(q)) : nothingOpen(), rulesCard())));
  }

  function item(q) {
    const a = agentOf(q.agent);
    const p = prec(q);
    const recs = p.records || [];
    const auto = String(q.answered_by || "").startsWith("rule:");
    const tag = q.state !== "open" ? (auto ? pill("auto", "good") : pill(q.answered_by === "you" ? "you" : q.answered_by || "answered", "mut"))
      : q.kind === "permission" ? pill("permission", "mut")
      : p.pending ? pill("asking ChloeCtx…", "mut")
      : matchIdx(q) >= 0 ? h("span", { class: "lv-pill lv-vio" }, "precedent found")
      : recs.length || (p.ids && p.ids.length) ? h("span", { class: "lv-pill lv-vio dim" }, "related memory")
      : pill("no precedent", "mut");
    const sub = q.state === "open" ? [a?.name, fmtDur(Date.now() - toMs(q.created))] : [a?.name, auto ? "answered by rule" : q.answered_by === "terminal" ? "answered in its terminal" : "answered by you", fmtClock(q.answered)];
    return h("button", { class: `de-it${S.sel === q.id ? " sel" : ""}`, "data-nav": "", "data-key": `q:${q.id}`, "aria-current": S.sel === q.id ? "true" : null, onclick: () => { S.sel = q.id; S.done = null; render(); } },
      a ? avatar(a, 26) : avatar({ name: "?" }, 26),
      h("div", { class: "de-it-b" }, h("b", {}, q.kind === "permission" ? permTitle(q, a ? a.name : "An agent") : q.text), q.kind === "permission" && q.context && !q.detail?.action ? h("code", { class: "de-it-c" }, firstLine(q.context)) : null, h("div", { class: "muted" }, sub.filter(Boolean).join(" · "))),
      tag);
  }
  const firstLine = (c) => String(c || "").split("\n")[0];

  function openList() {
    return h("section", { class: "de-open" },
      h("h2", { class: "lv-h" }, "Open", h("span", { class: "n" }, `· ${S.open.length}`)),
      S.open.length ? S.open.map(item) : h("div", { class: "de-none" }, "Nothing is waiting for you."));
  }

  function answeredCard() {
    const byRule = S.answered.filter((q) => String(q.answered_by || "").startsWith("rule:"));
    const byYou = S.answered.filter((q) => q.answered_by === "you").length;
    const byTerm = S.answered.filter((q) => q.answered_by === "terminal").length;
    return h("section", { class: "lv-card de-auto" },
      h("h2", { class: "lv-h" }, "Answered for you today", h("span", { class: "n" }, `· ${byRule.length}`)),
      h("div", { class: "de-auto-s" }, byRule.length ? "By rules you made. Each one shows the rule it used." : "No rule has answered a question today.",
        byYou || byTerm ? h("span", { class: "muted" }, ` You answered ${[byYou ? plural(byYou, "other") : null, byTerm ? `${byTerm} in a terminal` : null].filter(Boolean).join(" and ")}.`) : null),
      h("div", { class: "de-row" },
        S.answered.length ? h("button", { class: `btn sm${S.review ? " on" : ""}`, "data-key": "review", "aria-expanded": S.review ? "true" : "false", onclick: () => { S.review = !S.review; render(); } }, S.review ? "Only the last 3" : `Review today's ${S.answered.length}`) : null,
        h("button", { class: "btn sm", "data-key": "to-rules", onclick: () => rulesP.open() }, `Rules (${S.rules.length})`)),
      h("div", { class: "de-auto-l" }, (S.review ? S.answered : byRule.slice(0, 3)).map(item)));
  }

  function banner() {
    const d = S.done;
    if (!d) return null;
    return h("div", { class: `de-banner ${d.ok ? "ok" : "bad"}`, role: "status" }, d.ok ? "✓ " : "", h("b", {}, d.text), d.more ? ` ${d.more}.` : "",
      h("button", { class: "de-x", "data-key": "banner-x", "aria-label": "Dismiss", onclick: () => { S.done = null; render(); } }, "×"));
  }

  function asks(q) {
    const a = agentOf(q.agent);
    const chain = blocked(q);
    const waited = fmtDur(Date.now() - toMs(q.created));
    return h("div", { class: "de-who" }, a ? avatar(a, 26) : null,
      h("span", { class: "muted" },
        a ? h("button", { class: "de-link", "data-key": `ag:${q.id}`, onclick: () => ctx.go(`#/story/${a.id}`) }, a.name) : "An agent",
        q.kind === "permission" ? (q.detail?.menu?.kind === "trust" ? " asks before it starts" : " asks for permission") : " asks", ` · waiting ${waited}`,
        chain.length ? [" · blocks ", h("b", {}, chain[0].title), chain.length > 1 ? [", then ", h("b", {}, chain.slice(1).map((o) => o.title).join(", "))] : null] : null));
  }

  function detail(q) {
    if (q.kind === "permission") return permission(q);
    const os = opts(q);
    const pick = choiceOf(q);
    const m = matchIdx(q);
    const on = S.ruleOn.has(q.id) ? S.ruleOn.get(q.id) : m >= 0;
    const busy = S.sending === q.id;
    const a = agentOf(q.agent);
    const others = [...agents().values()].filter((x) => x.running !== false && x.id !== q.agent && (!q.repo || x.repo === q.repo)).map((x) => x.name);
    const ruleBox = h("textarea", { class: "de-rule-t", rows: 2, "data-key": `rt:${q.id}`, "aria-label": "The rule's text", disabled: !on });
    ruleBox.value = ruleFor(q, pick);
    ruleBox.addEventListener("input", () => { S.own.set(q.id, true); S.ruleText.set(q.id, ruleBox.value); });
    const words = h("textarea", { class: "de-words", rows: 2, "data-key": `w:${q.id}`, placeholder: `Tell ${a ? a.name : "it"} in your own words…`, "aria-label": "Your answer" });
    import("./complete.js").then((m) => m.attachComplete(words, { field: "answer", ctx, agent: q.agent })).catch(() => {}); // Fifth build: autocomplete
    const sendWords = () => { const t = words.value.trim(); if (t) answer(q, { text: t, ...(on ? { rule: ruleBox.value.trim() } : {}) }, t); };
    words.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) sendWords(); });
    const wordsOpen = S.choice.get(q.id) === "words";
    return h("section", { class: "lv-card de-q", "aria-label": "The selected question" },
      asks(q),
      h("h1", { class: "de-qt" }, inlineCode(q.text)),
      q.context ? h("div", { class: "de-ctx" }, inlineCode(q.context)) : null,
      h("div", { class: "de-opts", style: { "--n": Math.min(Math.max(os.length, 1), 3) } }, os.map((o, i) => h("button", {
        class: `de-opt${pick === i ? " on" : ""}${i === m ? " match" : ""}`, "data-nav": "", "data-key": `op:${q.id}:${i}`, "aria-pressed": pick === i ? "true" : "false",
        onclick: () => { S.choice.set(q.id, i); render(); },
      },
        h("div", { class: "de-opt-h" }, h("span", { class: "de-num" }, i + 1), h("b", {}, o.label),
          h("span", { class: "de-tags" }, i === m ? pill("matches your past choice", "good") : null, i === q.recommended && i !== m ? pill(`${a ? a.name : "it"} recommends`, "acc") : null)),
        o.detail ? h("div", { class: "de-opt-d" }, inlineCode(o.detail)) : null,
        o.estimate ? h("div", { class: "de-opt-e" }, o.estimate) : null))),
      precedent(q),
      h("div", { class: `de-rule${on ? " on" : ""}` },
        h("label", { class: "de-check" },
          h("input", { type: "checkbox", checked: on, "data-key": `rc:${q.id}`, onchange: (e) => { S.ruleOn.set(q.id, e.target.checked); render(); } }),
          h("span", { class: "de-box", "aria-hidden": "true" }),
          h("b", {}, "Make it a rule:")),
        h("div", { class: "de-rule-b" }, ruleBox, h("div", { class: "muted" }, "Every agent gets it from ChloeCtx; questions like this answer themselves."))),
      wordsOpen ? h("div", { class: "de-wbox" }, words, h("div", { class: "de-row" }, h("button", { class: "btn sm primary", "data-key": `ws:${q.id}`, disabled: busy, onclick: sendWords }, "Send my answer"), h("button", { class: "btn sm ghost", "data-key": `wc:${q.id}`, onclick: () => { S.choice.delete(q.id); render(); } }, "Cancel"), h("span", { class: "muted" }, "⌘↵ sends"))) : null,
      h("div", { class: "de-acts" },
        os.map((o, i) => h("button", { class: `btn${i === pick ? " primary" : ""}`, "data-key": `an:${q.id}:${i}`, disabled: busy, title: `Answer “${o.label}”`, onclick: () => answer(q, { option: i, ...(on && ruleBox.value.trim() ? { rule: ruleBox.value.trim() } : {}) }, o.label) }, busy && i === pick ? "Answering…" : o.label)),
        wordsOpen ? null : h("button", { class: "btn", "data-key": `aw:${q.id}`, onclick: () => { S.choice.set(q.id, "words"); render(); requestAnimationFrame(() => el.querySelector(".de-words")?.focus()); } }, "Answer in my words…"),
        h("span", { class: "de-note" }, `tells ${a ? a.name : "it"} now${on && others.length ? `; ${others.length > 3 ? `${others.slice(0, 3).join(", ")} and ${others.length - 3} more` : names(others)} get the rule` : ""}`)));
  }

  function permission(q) {
    const a = agentOf(q.agent);
    const name = a ? a.name : "the agent";
    const busy = S.sending === q.id;
    const os = opts(q);
    const d = q.detail || {};
    const m = d.menu || {};
    const hasNo = os.some((o) => o.no);
    const send = (i) => answer(q, { option: i, label: os[i].label }, os[i].label);
    return h("section", { class: "lv-card de-q de-perm", "aria-label": "The selected question" },
      asks(q),
      h("h1", { class: "de-qt" }, permTitle(q, name)),
      d.agent ? h("div", { class: "de-about" }, avatar({ name: d.agent.name }, 20), h("b", {}, d.agent.name),
        h("span", { class: "muted" }, [d.agent.running === false ? "exited" : d.agent.state === "waiting" ? "waiting for you" : d.agent.state, d.agent.outcome ? `outcome “${d.agent.outcome}”: ${String(d.agent.outcome_state || "").replace("_", " ")}` : null].filter(Boolean).join(" · "))) : null,
      d.description ? h("div", { class: "muted de-desc" }, d.description) : null,
      body(q, d),
      m.question ? h("div", { class: "de-pq" }, h("span", { class: "muted" }, `${name}'s prompt asks: `), h("b", {}, m.question)) : null,
      os.length ? h("div", { class: "de-popts", role: "group", "aria-label": "The prompt's options" }, os.map((o, i) => h("button", {
        class: `de-popt${o.no ? " no" : ""}${o.grants ? " more" : ""}${i === 0 && !o.no && !o.grants ? " yes" : ""}`, "data-nav": "", "data-key": `pm:${q.id}:${i}`, disabled: busy,
        title: `Choose “${o.label}” in ${name}'s prompt`, onclick: () => send(i),
      }, h("kbd", {}, o.n || i + 1), h("span", { class: "de-pl" }, h("b", {}, o.label), o.grants ? h("span", { class: "de-grants" }, `Grants more: ${o.grants}.`) : null))),
      hasNo ? null : h("button", { class: "de-popt no", "data-nav": "", "data-key": `pm:${q.id}:esc`, disabled: busy, title: "Esc: cancel the prompt", onclick: () => answer(q, { cancel: true }, "Cancel (Esc)") }, h("kbd", {}, "Esc"), h("span", { class: "de-pl" }, h("b", {}, "Cancel"))))
        : h("div", { class: "de-reading" }, h("i", { class: "de-spin" }), `Reading the prompt's options from ${name}'s terminal…`,
          a ? h("button", { class: "btn sm", "data-key": `pt:${q.id}`, onclick: () => ctx.go(`#/zoom/${a.id}`) }, "Open its terminal") : null),
      h("div", { class: "de-note de-perm-note" }, `Workbench types your choice into ${name}'s terminal only while this prompt is on its screen: it moves the cursor to that option, checks it there, then presses Enter. When the prompt has gone, it types nothing and closes this card.`));
  }

  // The whole command, or the edit's diff, from the hook that asked.
  function body(q, d) {
    if (d.diff) {
      return h("div", { class: "de-diffw" },
        h("div", { class: "de-file" }, h("code", {}, d.rel || d.file || ""), d.diff_lines ? h("span", { class: "muted" }, ` · ${d.diff_lines} line${d.diff_lines === 1 ? "" : "s"} changed`) : null),
        h("pre", { class: "de-diff", tabindex: "0", "aria-label": "The edit" }, String(d.diff).split("\n").map((l) => h("span", { class: l.startsWith("+") ? "add" : l.startsWith("-") ? "del" : l.startsWith("@@") || l === "…" || l.startsWith("(") ? "hunk" : "" }, l + "\n"))));
    }
    const cmd = d.command;
    if (cmd) return h("pre", { class: "de-cmd de-full", tabindex: "0", "aria-label": "The whole command" }, cmd);
    if (d.input && !d.action) return h("pre", { class: "de-cmd de-full", tabindex: "0" }, d.input);
    return q.context ? h("pre", { class: "de-cmd" }, q.context) : null;
  }

  function precedent(q) {
    const p = prec(q);
    const recs = p.records || [];
    if (!recs.length) {
      if (p.pending) return h("div", { class: "de-prec none" }, h("h2", { class: "lv-h" }, "Asking ChloeCtx…"), h("div", { class: "muted" }, "What was decided before about this shows here in a moment."));
      if (p.text && p.text.trim() && !/found nothing/i.test(p.text)) return h("div", { class: "de-prec" }, h("h2", { class: "lv-h" }, "ChloeCtx found"), h("pre", { class: "de-prec-raw" }, p.text.trim()));
      return h("div", { class: "de-prec none" }, h("h2", { class: "lv-h" }, "No precedent"), h("div", { class: "muted" }, p.missing.length ? p.missing.join(" ") : "ChloeCtx found nothing about this in memory. Your answer can be the first rule."));
    }
    const [top, ...rest] = recs;
    const mine = top.by === "you";
    const who = mine ? "you" : top.agent ? top.agent : { name: top.by || "ChloeCtx" };
    const files = (top.files || []).map((f) => f.split("/").pop());
    // A note's date has no time of day (ChloeCtx's ask prints dates).
    const when = (t) => (!t ? null : /^\d{4}-\d{2}-\d{2}$/.test(t) ? fmtDay(t) : `${fmtDay(t)}, ${fmtClock(t)}`);
    return h("div", { class: "de-prec" },
      h("h2", { class: "lv-h" }, mine ? "You decided this before" : matchIdx(q) >= 0 ? "Decided before" : "ChloeCtx remembers"),
      h("div", { class: "de-prec-r" }, avatar(who, 30),
        h("div", { class: "de-prec-b" },
          h("b", { class: "de-quote" }, "“", inlineCode(top.text), "”"),
          h("div", { class: "muted de-prec-m" },
            [mine ? "you, as a rule" : top.by || "a session before Workbench", when(top.ts), top.session_title ? `session “${top.session_title}”` : null, top.kind && top.kind !== "decision" ? top.kind : null].filter(Boolean).join(" · "),
            top.rewind ? [" · ", h("button", { class: "de-link acc", "data-key": `rwd:${q.id}`, onclick: () => ctx.openRewind(top.rewind) }, "Rewind ▶")] : null),
          files.length ? h("div", { class: "de-prec-f" }, "Applied since in ", files.map((f, i) => [i ? (i === files.length - 1 ? " and " : ", ") : "", h("code", { class: "lv-code" }, f)]), top.against === 0 ? "; no one has gone against it." : ".") : null)),
      rest.length ? h("div", { class: "de-prec-more" }, h("span", { class: "muted" }, "Also related: "), rest.map((r, i) => [i ? " · " : "", inlineCode(r.text), h("span", { class: "muted" }, ` (${[r.kind, r.by || r.agent?.name].filter(Boolean).join(", ")})`)])) : null);
  }

  function answeredDetail(q) {
    const a = agentOf(q.agent);
    const rid = String(q.answered_by || "").startsWith("rule:") ? q.answered_by.slice(5) : null;
    const r = rid && S.rules.find((x) => x.id === rid);
    return h("section", { class: "lv-card de-q de-done" },
      h("div", { class: "de-who" }, a ? avatar(a, 26) : null, h("span", { class: "muted" }, `${a ? a.name : "An agent"} asked at ${fmtClock(q.created)}`)),
      h("h1", { class: "de-qt" }, q.kind === "permission" ? permTitle(q, a ? a.name : "An agent") : inlineCode(q.text)),
      q.kind === "permission" ? body(q, q.detail || {}) : q.context ? h("div", { class: "de-ctx" }, inlineCode(q.context)) : null,
      h("div", { class: "de-ans" }, h("span", { class: "muted" }, "Answer"), h("b", {}, q.answer || "—"),
        h("span", { class: "muted" }, `${rid ? "by a rule" : q.answered_by === "terminal" ? "in its terminal" : "by you"} at ${fmtClock(q.answered)}`)),
      r ? h("div", { class: "de-prec" }, h("h2", { class: "lv-h" }, "The rule it used"),
        h("div", { class: "de-rr" }, inlineCode(r.text)),
        h("div", { class: "de-row" }, h("button", { class: "btn sm", "data-key": `rt-${r.id}`, onclick: () => toggleRule(r) }, r.active ? "Turn this rule off" : "Turn it back on"),
          h("span", { class: "muted" }, r.active ? "Turned off, questions like this one come to you again." : "It is off: questions like this one come to you."))) : null);
  }

  function nothingOpen() {
    return h("section", { class: "lv-card de-q de-calm" }, h("div", { class: "de-calm-i" }, "✓"), h("h1", { class: "de-qt" }, "Nothing waits for you."),
      h("div", { class: "muted" }, "When an agent asks (ask_human) or wants permission for a tool, the question comes here, with what ChloeCtx remembers about it."));
  }

  function rulesCard() {
    return rulesP.view();
  }


  // ------------------------------------------------------------- life
  const offKeys = keyNav(el, {
    onKey: (e) => {
      const q = S.open.find((x) => x.id === S.sel);
      if (!q || !/^[1-9]$/.test(e.key)) return false;
      const i = Number(e.key) - 1;
      if (q.kind === "permission") {
        // The prompt's own numbers: the key chooses the option it numbers.
        const os = opts(q);
        const at = os.findIndex((o, j) => (o.n || j + 1) === i + 1);
        if (at < 0 || S.sending) return false;
        answer(q, { option: at, label: os[at].label }, os[at].label);
        return true;
      }
      if (i >= opts(q).length) return false;
      S.choice.set(q.id, i);
      render();
      el.querySelector(`[data-key="op:${CSS.escape(q.id)}:${i}"]`)?.focus();
      return true;
    },
  });
  render();
  load();

  return {
    update(state) {
      if (!state) return;
      const before = new Map(S.stream.map((a) => [a.id, a.state]));
      S.stream = state.agents || S.stream;
      // A question asked or answered (its events), or an agent that starts
      // or stops waiting, is worth a reload.
      const evs = state.events || [];
      const last = evs.length ? evs[evs.length - 1].seq : 0;
      const asked = evs.some((e) => e.seq > (S.seq || 0) && (e.kind === "question" || e.kind === "answered" || e.kind === "Notification"));
      const was = S.seq;
      S.seq = Math.max(S.seq || 0, last || 0);
      if ((was != null && asked) || (S.stream || []).some((a) => before.get(a.id) !== a.state && (a.state === "waiting" || before.get(a.id) === "waiting"))) load();
      else render();
    },
    unmount() { alive = false; clearTimeout(S.again); offKeys(); el.remove(); },
  };
}

useStyle("decide", `
.de-wrap{padding:20px 22px 32px;max-width:1880px;margin:0 auto}
.de-head{display:flex;justify-content:space-between;align-items:flex-end;gap:20px;flex-wrap:wrap}
.de-head h1{margin:0;font-size:22px;font-weight:680;letter-spacing:-.012em}
.de-head .muted{margin-top:4px}
.de-tally{display:flex;gap:16px;color:var(--muted);font-size:12.5px}
.de-tally b{color:var(--text);font-weight:650;font-variant-numeric:tabular-nums}
.de-grid{display:grid;grid-template-columns:minmax(300px,410px) minmax(0,1fr);gap:20px;margin-top:16px;align-items:start}
.de-left,.de-right{display:flex;flex-direction:column;gap:12px;min-width:0}
.de-open .lv-h{margin:2px 0 10px}
.de-open{display:flex;flex-direction:column;gap:8px}.de-open>.lv-h{margin-bottom:2px}
.de-it{display:flex;gap:10px;align-items:flex-start;width:100%;text-align:left;font:inherit;color:inherit;padding:11px 12px;border:1px solid var(--line);border-radius:11px;background:var(--surface);cursor:pointer;box-shadow:var(--shadow-1);transition:border-color .12s,background .12s}
.de-it:hover{border-color:var(--line-2)}
.de-it.sel{border-color:color-mix(in srgb,var(--accent) 45%,var(--line));background:var(--accent-soft)}
.de-it-b{flex:1;min-width:0}.de-it-b b{display:block;font-weight:620;line-height:1.35}.de-it-b .muted{margin-top:2px;font-size:12.5px}
.de-it .lv-pill{flex:none}
.lv-vio.dim{opacity:.8}
.de-none{border:1px dashed var(--line-2);border-radius:11px;padding:16px;color:var(--muted);text-align:center}
.de-auto .lv-h{margin-bottom:8px}
.de-auto-s{color:var(--text-2);line-height:1.5}
.de-row{display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-top:10px}
.de-auto-l{display:flex;flex-direction:column;gap:6px;margin-top:12px}
.de-auto-l .de-it{box-shadow:none;padding:9px 10px;background:var(--surface-2)}
.de-auto-l .de-it.sel{background:var(--accent-soft)}
.de-banner{display:flex;align-items:center;gap:6px;flex-wrap:wrap;border-radius:10px;padding:9px 12px;font-size:13px;animation:rise .2s ease-out}
.de-banner.ok{background:var(--good-soft);color:var(--good-ink)}.de-banner.bad{background:var(--crit-soft);color:var(--crit-ink)}
.de-x{margin-left:auto;border:0;background:none;color:inherit;font-size:16px;line-height:1;cursor:pointer;border-radius:5px;padding:0 5px}
.de-q{padding:20px 22px}
.de-who{display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.de-who b{color:var(--text-2);font-weight:620}
.de-link{border:0;background:none;padding:0;font:inherit;color:var(--text-2);font-weight:600;cursor:pointer}
.de-link:hover{color:var(--accent)}.de-link.acc{color:var(--accent)}
.de-qt{margin:12px 0 0;font-size:20px;font-weight:680;letter-spacing:-.01em;line-height:1.3;text-wrap:balance}
.de-ctx{color:var(--text-2);margin-top:6px;line-height:1.5}
.de-opts{display:grid;grid-template-columns:repeat(var(--n),minmax(0,1fr));gap:12px;margin-top:16px}
.de-opt{display:flex;flex-direction:column;text-align:left;font:inherit;color:inherit;border:1px solid var(--line-2);border-radius:11px;padding:12px 14px 13px;background:var(--surface-2);cursor:pointer;transition:border-color .12s,box-shadow .12s}
.de-opt:hover{border-color:var(--muted)}
.de-opt.on{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus);background:var(--surface)}
.de-opt.on.match{border-color:var(--good);box-shadow:0 0 0 3px color-mix(in srgb,var(--good) 18%,transparent)}
.de-opt-h{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
.de-opt-h b{font-weight:650;font-size:14px}
.de-num{display:inline-grid;place-items:center;width:18px;height:18px;border-radius:5px;border:1px solid var(--line-2);font:600 10.5px var(--mono);color:var(--muted);flex:none}
.de-opt.on .de-num{background:var(--accent);border-color:var(--accent);color:var(--accent-ink)}
.de-opt.on.match .de-num{background:var(--good);border-color:var(--good);color:var(--surface)}
.de-tags{margin-left:auto;display:inline-flex;gap:5px}
.de-opt-d{color:var(--text-2);margin-top:7px;line-height:1.5}
.de-opt-e{color:var(--muted);margin-top:8px;font-size:12.5px}
.de-prec{margin-top:16px;border:1px solid color-mix(in srgb,var(--ctx) 35%,var(--line));background:linear-gradient(180deg,color-mix(in srgb,var(--ctx) 11%,var(--surface)),var(--surface));border-radius:12px;padding:14px 16px}
.de-prec>.lv-h{color:var(--ctx);margin-bottom:9px}
.de-prec.none{border-color:var(--line);background:var(--surface-2)}.de-prec.none>.lv-h{color:var(--muted);margin-bottom:4px}
.de-prec-r{display:flex;gap:12px;align-items:flex-start}
.de-prec-r .lv-you{font-size:10px}
.de-prec-b{min-width:0}
.de-quote{font-weight:650;font-size:14px;line-height:1.4}
.de-prec-m{margin-top:3px;font-size:12.5px}
.de-prec-f{color:var(--text-2);margin-top:8px}
.de-prec-more{margin-top:10px;padding-top:9px;border-top:1px solid color-mix(in srgb,var(--ctx) 20%,var(--line));font-size:12.5px;color:var(--text-2)}
.de-prec-raw{margin:0;white-space:pre-wrap;font:12px/1.5 var(--mono);color:var(--text-2)}
.de-rule{display:grid;grid-template-columns:auto minmax(0,1fr);gap:4px 10px;align-items:start;margin-top:16px}
.de-check{display:inline-flex;align-items:center;gap:10px;cursor:pointer;padding-top:3px;white-space:nowrap}
.de-check input{position:absolute;opacity:0;width:1px;height:1px}
.de-box{width:18px;height:18px;border-radius:5px;border:1.5px solid var(--line-2);background:var(--surface);display:grid;place-items:center;flex:none;transition:background .12s,border-color .12s}
.de-check input:checked+.de-box{background:var(--accent);border-color:var(--accent)}
.de-check input:checked+.de-box::after{content:"";width:9px;height:5px;border-left:2px solid var(--accent-ink);border-bottom:2px solid var(--accent-ink);transform:translateY(-1px) rotate(-45deg)}
.de-check input:focus-visible+.de-box{outline:2px solid var(--accent);outline-offset:2px}
.de-rule-b{min-width:0}
.de-rule-b .muted{font-size:12.5px;margin-top:2px}
.de-rule-t{display:block;width:100%;border:1px solid transparent;border-radius:7px;background:transparent;color:var(--text-2);font:inherit;line-height:1.45;resize:none;padding:2px 6px;margin:0 0 0 -6px;overflow:hidden;font-style:italic;field-sizing:content;min-height:0}
.de-rule.on .de-rule-t{color:var(--text);font-style:normal}
.de-rule-t:hover:not(:disabled){border-color:var(--line-2)}
.de-rule-t:focus{outline:none;border-color:var(--accent);box-shadow:0 0 0 3px var(--focus);background:var(--surface-2)}
.de-rule-t:disabled{opacity:.6}
.de-wbox{margin-top:14px}
.de-words{width:100%;border:1px solid var(--line-2);border-radius:9px;background:var(--surface-2);color:var(--text);font:inherit;padding:9px 11px;resize:vertical;outline:none}
.de-words:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.de-acts{display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-top:18px}
.de-acts .btn kbd{margin-left:4px}
.de-note{margin-left:auto;color:var(--muted);font-size:12.5px;text-align:right}
.de-perm .de-perm-b{margin-top:12px}
.de-perm-t b{font-size:15px;font-weight:650}
.de-cmd{margin:10px 0 0;font:12.5px/1.5 var(--mono);background:var(--sunken);border:1px solid var(--line);border-radius:8px;padding:9px 12px;white-space:pre-wrap;overflow-wrap:anywhere;color:var(--text)}
.de-perm .de-acts{margin-top:12px}
.de-it-c{display:block;font:11.5px/1.4 var(--mono);color:var(--text-2);margin-top:3px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.de-about{display:flex;align-items:center;gap:8px;margin-top:8px;font-size:13px}
.de-desc{margin-top:6px}
.de-full{max-height:min(46vh,420px);overflow:auto;white-space:pre;overflow-wrap:normal;tab-size:4}
.de-diffw{margin-top:10px;border:1px solid var(--line);border-radius:8px;overflow:hidden}
.de-file{padding:6px 12px;background:var(--surface-2);border-bottom:1px solid var(--line);font-size:12.5px}
.de-file code{font:600 12px var(--mono)}
.de-diff{margin:0;max-height:min(46vh,420px);overflow:auto;font:12.5px/1.5 var(--mono);background:var(--sunken);padding:6px 0;tab-size:4}
.de-diff span{display:block;padding:0 12px;white-space:pre}
.de-diff .add{background:var(--good-soft);color:var(--good-ink)}
.de-diff .del{background:var(--crit-soft);color:var(--crit-ink)}
.de-diff .hunk{color:var(--muted)}
.de-pq{margin-top:14px;font-size:13px}
.de-popts{display:flex;flex-direction:column;gap:6px;margin-top:8px}
.de-popt{display:flex;align-items:flex-start;gap:10px;width:100%;text-align:left;font:inherit;color:inherit;padding:9px 12px;border:1px solid var(--line-2);border-radius:9px;background:var(--surface);cursor:pointer;transition:border-color .12s,background .12s}
.de-popt:hover:not(:disabled){border-color:var(--muted)}
.de-popt:focus-visible{outline:none;border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.de-popt:disabled{opacity:.55;cursor:default}
.de-popt kbd{flex:none;margin-top:1px}
.de-popt.yes{border-color:color-mix(in srgb,var(--accent) 55%,var(--line));background:var(--accent-soft)}
.de-popt.no b{color:var(--crit-ink)}
.de-popt.more{border-style:dashed}
.de-pl{display:flex;flex-direction:column;gap:2px;min-width:0;overflow-wrap:anywhere}
.de-pl b{font-weight:620;line-height:1.4}
.de-grants{font-size:12px;color:var(--warn-ink)}
.de-reading{display:flex;align-items:center;gap:10px;flex-wrap:wrap;margin-top:12px;color:var(--text-2)}
.de-spin{width:12px;height:12px;border-radius:50%;border:2px solid var(--line-2);border-top-color:var(--accent);animation:de-rot .8s linear infinite}
@keyframes de-rot{to{transform:rotate(360deg)}}
.de-perm-note{margin:12px 0 0;text-align:left;max-width:none}
.de-ans{display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;margin-top:14px;padding:10px 12px;border-radius:9px;background:var(--surface-2)}
.de-ans b{font-size:14px}
.de-rr{font-weight:620;line-height:1.45}
.de-calm{text-align:center;padding:32px}.de-calm-i{width:38px;height:38px;border-radius:50%;margin:0 auto;display:grid;place-items:center;background:var(--good-soft);color:var(--good-ink);font-weight:800}
.de-calm .muted{max-width:520px;margin:6px auto 0}
.de-rules-h{display:flex;justify-content:space-between;align-items:baseline;gap:12px;flex-wrap:wrap;margin-bottom:6px}
.de-rules-h .lv-h{margin:0}.de-rules-h .muted{font-size:12.5px}
.de-rules ul{list-style:none;margin:0;padding:0;display:grid;grid-template-columns:repeat(auto-fill,minmax(360px,1fr));column-gap:22px}
.de-rules li{display:flex;gap:11px;align-items:flex-start;padding:9px 0;border-top:1px solid var(--line)}
.de-rules li.off .de-rule-x>div:first-child{color:var(--muted);text-decoration:line-through;text-decoration-color:var(--line-2)}
.de-rules li.fresh{animation:de-fresh 2.4s ease-out}
@keyframes de-fresh{0%{background:var(--accent-soft)}100%{background:transparent}}
.de-rule-x{min-width:0;line-height:1.4}.de-rule-x .muted{font-size:12px;margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.de-sw{flex:none;width:30px;height:18px;border-radius:999px;border:0;background:var(--line-2);position:relative;cursor:pointer;margin-top:1px;transition:background .15s;padding:0}
.de-sw i{position:absolute;top:2px;left:2px;width:14px;height:14px;border-radius:50%;background:var(--surface);box-shadow:0 1px 2px rgba(0,0,0,.25);transition:transform .15s}
.de-sw[aria-checked=true]{background:var(--good)}.de-sw[aria-checked=true] i{transform:translateX(12px)}
.de-skel{height:300px;border-radius:12px;background:var(--surface-3);margin-top:16px;animation:pulse 1.4s ease-in-out infinite}
.de-err{margin-top:16px;display:flex;flex-direction:column;gap:8px;align-items:flex-start;max-width:560px}
@media (max-width:1000px){.de-grid{grid-template-columns:minmax(0,1fr)}}
@media (max-width:640px){
  .de-wrap{padding:14px 14px 24px}
  .de-q{padding:16px}
  .de-opts{grid-template-columns:minmax(0,1fr)}
  .de-qt{font-size:17px}
  .de-tally{gap:10px;flex-wrap:wrap}
  .de-rule{grid-template-columns:minmax(0,1fr)}
  .de-note{margin-left:0;text-align:left}
  .de-rules ul{grid-template-columns:minmax(0,1fr)}
}
`);
