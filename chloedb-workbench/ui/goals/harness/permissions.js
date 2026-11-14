// Optimize the harness, the permissions editor (HARNESS.md, "Screens", 4):
// the rules agents of the repository get, each with this week's hits; a
// rule typed with completions from the commands agents ran; lint
// (unreachable, conflicting, wider than needed, secrets); the structured
// view and the raw JSON kept in step both ways; the counted impact on the
// last 7 days' calls before saving; Save to a draft and Promote, a widening
// confirmed by naming each pattern.
import { h, useStyle, page, label, btn, bank, icon } from "/levels/common.js";
import { grantSheet } from "/goals/rules-suggest.js";
import { tabs, lv, gauge, fmtMs, fmtWhen, rich, repoQ, repoName, toast, failed, loading, seg, resultLine, saveDraft, promote } from "./parts.js";

const EMPTY = () => ({ allow: [], ask: [], deny: [], remove: [] });

export function render(host, ctx) {
  host.replaceChildren(loading());
  const S = { alive: true, view: matchMedia("(max-width:1024px)").matches ? "structured" : "split", seq: 0, dirty: false };
  const load = () => ctx.api(`/api/harness/permissions?${repoQ(ctx)}`).then((d) => {
    if (!S.alive) return;
    Object.assign(S, { d, repo: d.repo, v: d.version, edits: { ...EMPTY(), ...d.edits }, rules: d.rules, flags: d.lint, impact: d.impact, widens: d.widens, text: d.block, textOk: true, dirty: false });
    // From the Coach ("Make this a harness change"): its rules prefilled in
    // the draft, not saved; Save and Promote stay yours.
    // Read from the address itself: the shell may keep this page mounted
    // across a change of its query.
    const q = new URLSearchParams(String(location.hash || "").split("?")[1] || "");
    const want = q.getAll("allow").map((x) => x.trim()).filter(Boolean);
    if (want.length && !S.prefillDone) {
      S.prefillDone = true;
      S.prefilled = want.filter((t) => !S.rules.some((r) => r.decision === "allow" && r.text === t) && !S.edits.allow.includes(t));
      S.prefillFrom = q.get("from") || "";
      for (const t of S.prefilled) S.edits.allow.push(t);
      draw();
      if (S.prefilled.length) fromEdits();
      return;
    }
    draw();
  }).catch((err) => { if (S.alive) host.replaceChildren(failed(err)); });

  // The structured edits changed: lint, the JSON, the impact.
  async function fromEdits() {
    const n = ++S.seq;
    S.dirty = true;
    try {
      const r = await ctx.api("/api/harness/lint", { method: "POST", body: { repo: S.repo, edits: S.edits } });
      if (n !== S.seq) return;
      S.rules = r.rules; S.flags = r.flags; S.text = blockText(r.rules); S.textOk = true;
      draw();
      await impact(n);
    } catch (err) { toast(err.message, "bad"); }
  }
  // The JSON changed: parse it back into edits.
  async function fromText(text) {
    const n = ++S.seq;
    S.dirty = true;
    S.text = text;
    try {
      const r = await ctx.api("/api/harness/lint", { method: "POST", body: { repo: S.repo, text } });
      if (n !== S.seq) return;
      if (!r.ok) { S.textOk = false; S.parseError = r.error; drawSide(); syncMark(); return; }
      S.textOk = true; S.parseError = null; S.edits = { ...EMPTY(), ...r.edits }; S.rules = r.rules; S.flags = r.flags;
      drawRules(); drawSide(); syncMark();
      await impact(n);
    } catch (err) { toast(err.message, "bad"); }
  }
  async function impact(n) {
    const r = await ctx.api("/api/harness/impact", { method: "POST", body: { repo: S.repo, edits: S.edits } });
    if (n !== S.seq) return;
    S.impact = r; S.widens = r.widens;
    drawSide();
  }
  async function save() {
    const r = await saveDraft(ctx, S.repo, S.v.id, { permissions: S.edits });
    S.v = r.version; S.dirty = false; S.prefilled = null;
    toast(`Saved to ${r.version.name}.`);
    return r.version;
  }
  async function doPromote() {
    try {
      const v = S.dirty || S.v.state === "current" ? await save() : S.v;
      if (v.state === "current") { toast("Nothing to promote: this is the current version."); return; }
      const r = await promote(ctx, v.id);
      if (!r) { toast("Not promoted: the widening was not confirmed."); return; }
      toast(`${r.version.name} is current: new agents get it.`);
      load();
    } catch (err) { toast(err.status === 409 && err.code === "unchanged" ? "Nothing changed from the current version." : err.message, "bad"); }
  }

  let rulesBox, sideBox, textArea, syncEl, mainCols;
  function syncMark() {
    if (!syncEl) return;
    syncEl.replaceChildren(S.textOk ? h("span", { class: "hx-ok" }, icon("check", "i sm"), "in sync") : h("span", { class: "hx-bad" }, icon("warn", "i sm"), "not parsed: the rules show the last good text"));
  }
  function drawRules() {
    if (!rulesBox) return;
    rulesBox.replaceChildren(rulesTable(S, ctx, {
      remove(r) {
        const mine = S.edits[r.decision] || [];
        const i = mine.indexOf(r.text);
        if (i >= 0) mine.splice(i, 1); else if (!S.edits.remove.includes(r.text)) S.edits.remove.push(r.text);
        fromEdits();
      },
      async add(decision, text) {
        text = text.trim();
        if (!text) return;
        const put = () => {
          const ri = S.edits.remove.indexOf(text);
          if (ri >= 0) S.edits.remove.splice(ri, 1);
          if (!S.rules.some((r) => r.decision === decision && r.text === text) && !S.edits[decision].includes(text)) S.edits[decision].push(text);
          fromEdits();
          return true;
        };
        // An allow passes Rules' grant sheet first (src/rulesuggest.rs: what
        // else it would allow, risky first); it goes into the draft only.
        if (decision !== "allow") { put(); return; }
        const r = await grantSheet(ctx, { pattern: text, repo: S.repo, file: `${S.v.state === "draft" ? S.v.name : "a new draft"}: each new agent's own --settings, once promoted`, title: "This rule goes into the draft", onConfirm: async () => put() });
        if (r && r.choice === "narrow") toast(`Rules saved ${r.pattern} for new agents of this project.`);
      },
      undoRemove(text) { S.edits.remove = S.edits.remove.filter((t) => t !== text); fromEdits(); },
    }));
  }
  function drawSide() {
    if (!sideBox) return;
    sideBox.replaceChildren(...side(S, { save: () => save().then(() => draw()).catch((err) => toast(err.message, "bad")), promote: doPromote }));
  }
  function draw() {
    const d = S.d;
    rulesBox = h("div", { class: "hx-rules" });
    textArea = h("textarea", { class: "field hx-json", spellcheck: "false", "aria-label": "The permissions block, as JSON", rows: "18" });
    textArea.value = S.text;
    let timer = null;
    textArea.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(() => fromText(textArea.value), 350); });
    syncEl = h("span", { class: "hx-sync" });
    const textPane = h("div", { class: "pane hx-textpane" },
      h("div", { class: "hd" }, label("settings.json · permissions", h("span", { class: "sp" }), syncEl)),
      h("div", { class: "bd" }, textArea),
      h("p", { class: "hx-note hx-pad" }, "The block Workbench writes into each new session's own --settings, with ", h("code", {}, "<worktree>"), " for its worktree. Yours in ~/.claude are read, never written."));
    mainCols = h("div", { class: `hx-ed ${S.view}` }, h("div", { class: "pane hx-rpane" }, rulesBox), textPane);
    sideBox = h("div", { class: "hx-side" });
    const sub = `What agents of ${repoName(S.repo)} may do without asking, must ask for, or may never do. Hits are the last 7 days' ${d.calls} tool calls each rule decided; ${d.prompts} of them asked you.`;
    const pre = S.prefilled ? h("div", { class: "callout hx-prefill", role: "status" }, icon("check", "i"), h("div", {},
      h("b", {}, S.prefilled.length ? `${S.prefilled.length === 1 ? "1 rule" : `${S.prefilled.length} rules`} from ${S.prefillFrom === "coach" ? "Where the time went" : "the Coach"} in the draft` : "Those rules are already here"),
      h("p", {}, S.prefilled.length ? [S.prefilled.map((t, i) => [i ? ", " : "", h("code", {}, t)]), ". Nothing is saved yet: check the impact on the right, then Save to a draft and Promote. Your ~/.claude settings are not touched."] : "Every rule the Coach suggested is already allowed in this version."))) : null;
    host.replaceChildren(page({ kicker: `Optimize the harness · editing ${S.v.name}${S.v.state === "draft" ? `, from ${d.current.name}` : " (current)"}`, title: "Permissions", sub, inspector: sideBox, cls: "hx-page" },
      tabs("permissions"),
      pre,
      h("section", {},
        label(h("span", {}, "Rules ", h("span", { class: "hx-num" }, String(S.rules.length))), h("span", { class: "hx-mut hx-rule-word" }, "deny wins over ask, ask over allow"),
          seg([["structured", "Structured"], ["split", "Split"], ["text", "Text"]], S.view, (k) => { S.view = k; mainCols.className = `hx-ed ${k}`; draw(); }, { label: "View" })),
        mainCols)));
    drawRules(); drawSide(); syncMark();
  }
  load();
  return { unmount() { S.alive = false; } };
}

function blockText(rules) {
  const b = {};
  for (const d of ["deny", "ask", "allow"]) {
    const l = rules.filter((r) => r.decision === d).map((r) => r.text);
    if (l.length) b[d] = l;
  }
  return JSON.stringify({ permissions: b }, null, 2);
}

function rulesTable(S, ctx, act) {
  const flagsOf = (r) => (S.flags || []).filter((f) => f.pattern === r.text);
  const removed = S.edits.remove || [];
  const decision = h("select", { class: "field hx-dec", "aria-label": "Decision" }, ["allow", "ask", "deny"].map((x) => h("option", { value: x }, x)));
  const input = h("input", { class: "field hx-add", placeholder: "Bash(npm test:*) · Edit(<worktree>/docs/**) · mcp__server", autocomplete: "off", spellcheck: "false", "aria-label": "A new rule" });
  const list = h("div", { class: "hx-ac", role: "listbox", hidden: true });
  let items = [], at = 0, timer = null;
  const pick = (i) => { const it = items[i]; if (!it) return; input.value = it.insert; list.hidden = true; input.focus(); };
  const show = () => {
    list.replaceChildren(
      h("div", { class: "hx-ach" }, h("span", {}, "Ran here · 7 days"), h("span", {}, "asked · allowed · denied")),
      ...items.map((it, i) => h("button", { type: "button", role: "option", class: `hx-aci${i === at ? " on" : ""}`, "aria-selected": String(i === at), onmousedown: (e) => { e.preventDefault(); pick(i); } },
        h("span", { class: "hx-acl" }, h("code", {}, it.label), h("small", {}, it.note || `${it.agents} agent${it.agents === 1 ? "" : "s"} · ${it.runs} run${it.runs === 1 ? "" : "s"}${it.saves ? ` · saves ${it.saves} prompt${it.saves === 1 ? "" : "s"}` : ""}`)),
        h("span", { class: "hx-num hx-acn" }, `${it.asked} · ${it.allowed} · ${it.denied}`))),
      h("div", { class: "hx-acf" }, h("kbd", { class: "kc" }, "↑"), h("kbd", { class: "kc" }, "↓"), " pick ", h("kbd", { class: "kc" }, "Tab"), " take · only commands that ran here"));
    list.hidden = !items.length;
  };
  const complete = async () => {
    const q = input.value.replace(/^Bash\(/, "").replace(/(:\*)?\)?$/, "");
    if (!q.trim() || /^(Edit|Read|Write|WebFetch|mcp__)/.test(input.value)) { items = []; show(); return; }
    try {
      const r = await ctx.api(`/api/harness/complete?repo=${encodeURIComponent(S.repo)}&kind=command&q=${encodeURIComponent(q)}`);
      items = (r.items || []).slice(0, 6); at = 0; show();
    } catch { items = []; show(); }
  };
  input.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(complete, 120); });
  input.addEventListener("blur", () => setTimeout(() => { list.hidden = true; }, 120));
  input.addEventListener("keydown", (e) => {
    if (!list.hidden && items.length && (e.key === "ArrowDown" || e.key === "ArrowUp")) { e.preventDefault(); at = (at + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length; show(); return; }
    if (!list.hidden && items.length && e.key === "Tab") { e.preventDefault(); pick(at); return; }
    if (e.key === "Escape") { list.hidden = true; return; }
    if (e.key === "Enter") { e.preventDefault(); act.add(decision.value, input.value); }
  });
  const order = { deny: 0, ask: 1, allow: 2 };
  const rows = [...S.rules].sort((a, b) => order[a.decision] - order[b.decision]);
  return h("div", {},
    h("table", { class: "hx-t hx-rt" },
      h("thead", {}, h("tr", {}, h("th", {}, "Decide"), h("th", { "data-k": "rule" }, "Rule"), h("th", { "data-k": "from" }, "From"), h("th", { class: "r" }, "Hits 7d"), h("th", { class: "r", "data-k": "last" }, "Last"), h("th", {}, h("span", { class: "hx-sr" }, "Remove")))),
      h("tbody", {}, rows.map((r) => {
        const fl = flagsOf(r);
        return h("tr", { class: fl.some((f) => f.severity !== "info") ? "hx-flagged" : null },
          h("td", { "data-k": "dec" }, h("span", { class: `hx-dc ${r.decision}` }, r.decision)),
          h("td", { "data-k": "rule" }, h("code", { class: "hx-rule" }, r.text), fl.map((f) => h("div", { class: `hx-lf ${f.severity}` }, icon("warn", "i sm"), { unreachable: "never matches", wide: "wider than needed", conflict: "conflicts", shadowed: "shadowed", widens: "widens", secret: "secret" }[f.rule] || f.rule))),
          h("td", { "data-k": "from", class: "hx-mut" }, r.from),
          h("td", { class: "r hx-num", "data-k": "hits" }, String(r.hits)),
          h("td", { class: "r hx-num hx-mut", "data-k": "last" }, r.last ? fmtWhen(r.last) : "–"),
          h("td", { "data-k": "x" }, h("button", { type: "button", class: "hx-x", title: `Remove ${r.text}`, "aria-label": `Remove ${r.text}`, onclick: () => act.remove(r) }, icon("x", "i sm"))));
      }))),
    removed.length ? h("div", { class: "hx-removed" }, h("span", { class: "lbl" }, "Taken out of Workbench's"), removed.map((t) => h("button", { type: "button", class: "hx-chip", onclick: () => act.undoRemove(t), title: "Put it back" }, h("code", {}, t), icon("x", "i sm")))) : null,
    h("div", { class: "hx-addrow" }, decision, h("div", { class: "hx-addw" }, input, list), h("button", { type: "button", class: "btn", onclick: () => act.add(decision.value, input.value) }, "Add")));
}

function side(S, act) {
  const flags = (S.flags || []).filter((f) => f.severity !== "info" || f.rule === "shadowed");
  const im = S.impact || {};
  const w = S.widens || {};
  const changed = S.dirty || S.v.state === "draft";
  const lint = h("div", {},
    label("Lint", h("span", { class: "hx-num" }, String(flags.length))),
    flags.length ? h("div", { class: "hx-lint" }, flags.map((f) => h("div", { class: `hx-li ${f.severity}` },
      icon(f.severity === "bad" ? "x" : f.severity === "info" ? "why" : "warn"),
      h("div", {}, h("b", {}, { unreachable: "Never matches", wide: "Wider than needed", conflict: "Conflicts", shadowed: "Shadowed", widens: "Widens", secret: "A secret in the harness" }[f.rule] || f.rule, f.pattern ? [": ", h("code", {}, f.pattern)] : null),
        h("p", {}, rich(f.message)),
        f.fix ? h("div", { class: "hx-fix" }, (Array.isArray(f.fix) ? f.fix : [f.fix]).map((x) => h("code", {}, x))) : null))))
      : h("p", { class: "hx-note" }, "Nothing to flag: every rule is reachable, none conflicts, none is wider than the calls it decided, and no secret is in the block or the environment."));
  if (S.parseError) lint.append(h("div", { class: "hx-li bad" }, icon("x"), h("div", {}, h("b", {}, "The JSON does not parse"), h("p", {}, S.parseError))));
  const imp = h("div", {},
    label("Impact of the change", h("span", { class: "sp" }), lv(im.level || "counted")),
    h("div", { class: "hx-gap" }, bank(2,
      gauge({ label: "Prompts", v: `${im.prompts_before ?? 0} → ${im.prompts_after ?? 0}`, on: im.prompts_after, total: Math.max(im.prompts_before || 0, im.prompts_after || 0), tone: (im.prompts_after || 0) > (im.prompts_before || 0) ? "warn" : "", delta: im.saved ? `${im.saved} would not have asked` : im.added ? `${im.added} more would ask` : "no prompt changes" }),
      gauge({ label: "Denies changed", v: String(im.denies_changed || 0), on: im.denies_changed, total: Math.max(1, im.denies_changed || 0), tone: im.denies_changed ? "warn" : "", delta: im.denies_changed ? "calls that were denied would not be" : "every deny holds" }))),
    im.result === "no_difference"
      ? resultLine("no_difference", `This version decides each of the ${im.cases ?? im.calls ?? 0} calls of the last 7 days as the current one does.`)
      : h("p", { class: "hx-note" }, `Replayed against ${im.calls ?? 0} tool calls of the last 7 days: ${im.saved || 0} calls you allowed by hand would have run without asking${im.wait_saved_ms ? `, and ${fmtMs(im.wait_saved_ms)} of waiting would not have happened` : ""}${im.added ? `; ${im.added} would ask that did not` : ""}.${im.already ? ` ${im.already} more asked under an older version and the current rules allow already.` : ""}`),
    h("p", { class: "hx-note" }, im.note || "Exact over the calls the window recorded."));
  const wide = w.widens ? h("div", { class: "hx-widens" }, icon("warn"),
    h("div", {}, h("b", {}, "It widens what agents may do. "), "Promoting it waits for you to confirm, by name: ", (w.patterns || []).map((p, i) => [i ? ", " : "", h("code", {}, p)]), ".")) : null;
  const dock = h("div", { class: "hx-dock" },
    btn(S.v.state === "draft" && !S.dirty ? `Saved in ${S.v.name}` : `Save to ${S.v.state === "draft" ? S.v.name : "a draft"}`, { primary: true, kbd: "⌘S", disabled: !S.dirty || !S.textOk, onclick: act.save }),
    btn(w.widens ? "Promote… (confirm the widening)" : "Promote: new agents get it", { disabled: !changed || !S.textOk, onclick: act.promote }));
  return [lint, imp, wide, dock].filter(Boolean);
}

useStyle("harness-perms", `
.hx-ed{display:grid;gap:var(--s-4);align-items:start;margin-top:var(--s-3)}
.hx-ed.split{grid-template-columns:minmax(0,1.25fr) minmax(0,1fr)}
.hx-ed.structured .hx-textpane,.hx-ed.text .hx-rpane{display:none}
.hx-rule-word{text-transform:none;letter-spacing:0;font-family:var(--sans);font-size:var(--fs-sm)}
.hx-rt td{vertical-align:middle}
.hx-rt td[data-k=rule]{overflow-wrap:anywhere}
.hx-rt th[data-k=rule]{width:45%}
.hx-ed.split .hx-rt [data-k=last]{display:none}
.hx-rt tr.hx-flagged td{background:var(--warn-soft)}
.hx-dc{display:inline-block;font:var(--w-mid) var(--fs-xs)/18px var(--mono);letter-spacing:.06em;text-transform:uppercase;padding:0 6px;border-radius:var(--r-xs)}
.hx-dc.deny{color:var(--crit-ink);background:var(--crit-soft)}.hx-dc.ask{color:var(--warn-ink);background:var(--warn-soft)}.hx-dc.allow{color:var(--good-ink);background:var(--good-soft)}
.hx-rule{font-size:var(--fs-body)}
.hx-lf{display:flex;align-items:center;gap:4px;margin-top:4px;font:var(--w-mid) var(--fs-xs)/16px var(--mono);letter-spacing:.05em;text-transform:uppercase;color:var(--warn-ink)}
.hx-lf.bad{color:var(--crit-ink)}.hx-lf.info{color:var(--text-2)}
.hx-x{display:inline-grid;place-items:center;width:32px;height:32px;border:0;border-radius:var(--r-sm);background:none;color:var(--text-3);cursor:pointer}
.hx-x:hover{background:var(--surface-2);color:var(--crit-ink)}
.hx-sr{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)}
.hx-removed{display:flex;flex-wrap:wrap;gap:var(--s-2);align-items:center;padding:var(--s-3) var(--s-4);border-top:var(--hair) solid var(--line)}
.hx-chip{display:inline-flex;align-items:center;gap:6px;min-height:28px;padding:0 8px;border:var(--hair) solid var(--line-2);border-radius:var(--r-xs);background:var(--del-bg);color:var(--del-ink);cursor:pointer;text-decoration:line-through}
.hx-addrow{display:flex;gap:var(--s-2);align-items:flex-start;padding:var(--s-3) var(--s-4);border-top:var(--hair) solid var(--line);background:var(--accent-soft)}
.hx-dec{width:auto;flex:none;font-family:var(--mono)}
.hx-addw{position:relative;flex:1;min-width:0}
.hx-add{font-family:var(--mono)}
.hx-ac{position:absolute;left:0;right:0;top:calc(100% + 6px);z-index:20;background:var(--surface);border:var(--hair) solid var(--line-2);border-radius:var(--r);box-shadow:var(--shadow-2);overflow:hidden}
.hx-ac[hidden]{display:none}
.hx-ach,.hx-acf{display:flex;justify-content:space-between;gap:var(--s-3);padding:var(--s-2) var(--s-4);font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2)}
.hx-acf{justify-content:flex-start;align-items:center;gap:6px;text-transform:none;letter-spacing:0;font-family:var(--sans);border-top:var(--hair) solid var(--line)}
.hx-aci{display:flex;align-items:center;gap:var(--s-3);width:100%;min-height:44px;padding:var(--s-2) var(--s-4);border:0;border-top:var(--hair) solid var(--line);background:none;color:var(--text);text-align:left;cursor:pointer}
.hx-aci.on{background:var(--accent-soft);box-shadow:inset 2px 0 0 var(--accent)}
.hx-acl{display:flex;flex-direction:column;min-width:0;flex:1}.hx-acl code{overflow-wrap:anywhere}
.hx-acl small{color:var(--text-2);font-size:var(--fs-sm)}
.hx-acn{flex:none;font-size:var(--fs-sm)}
.hx-json{font-family:var(--mono);font-size:var(--fs-sm);line-height:22px;min-height:420px;white-space:pre;tab-size:2}
.hx-pad{padding:0 var(--s-4) var(--s-3)}
.hx-sync{font:var(--w-mid) var(--fs-xs)/16px var(--mono);text-transform:none;letter-spacing:0}
.hx-ok{display:inline-flex;gap:4px;align-items:center;color:var(--good-ink)}.hx-bad{display:inline-flex;gap:4px;align-items:center;color:var(--crit-ink)}
.hx-lint{display:flex;flex-direction:column;margin-top:var(--s-2)}
.hx-li{display:grid;grid-template-columns:18px minmax(0,1fr);gap:var(--s-3);padding:var(--s-3) 0;border-bottom:var(--hair) solid var(--line)}
.hx-li:last-child{border-bottom:0}
.hx-li>svg{color:var(--warn);margin-top:2px}.hx-li.bad>svg{color:var(--crit)}.hx-li.info>svg{color:var(--text-2)}
.hx-li b{font-weight:var(--w-mid);overflow-wrap:anywhere}
.hx-li p{margin:4px 0 0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm);overflow-wrap:anywhere}
.hx-fix{display:flex;flex-wrap:wrap;gap:6px;margin-top:6px}.hx-fix code{padding:1px 6px;border-radius:var(--r-xs);background:var(--good-soft);color:var(--good-ink)}
.hx-widens{display:grid;grid-template-columns:18px minmax(0,1fr);gap:var(--s-3);padding:var(--s-3) var(--s-4);border-radius:var(--r-sm);background:var(--warn-soft);color:var(--text);font-size:var(--fs-sm);line-height:var(--lh-sm);overflow-wrap:anywhere}
.hx-widens>svg{color:var(--warn)}
@media (max-width:1024px){.hx-ed.split{grid-template-columns:minmax(0,1fr)}}
@media (max-width:640px){
  .hx-rule-word{display:none}
  .hx-rt tr{grid-template-columns:auto minmax(0,1fr) auto !important}
  .hx-rt td[data-k=from],.hx-rt td[data-k=last]{display:none !important}
  .hx-rt td[data-k=hits]{grid-column:2;text-align:left !important;color:var(--text-2)}
  .hx-rt td[data-k=hits]::after{content:" hits this week"}
  .hx-rt td[data-k=x]{grid-row:1 / span 2;grid-column:3;align-self:center}
  .hx-addrow{flex-wrap:wrap}.hx-addw{flex-basis:100%;order:3}
  .hx-x{width:44px;height:44px}
}
`);
