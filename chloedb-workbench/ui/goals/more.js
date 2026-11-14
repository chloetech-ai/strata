// More (JOURNEYS.md, "More"): everything that is not one of the four goals.
// #/more is the list (the phone's More tab); #/more/<item> is one of them.
// All terminals and the Decisions timeline are app.js's own views; the
// others mount here: Where the time went (flow), Library, Rules and grants,
// Guide, Keys, Setup check and Settings.
import { h, useStyle, page, btn, label, kc, pane, icon, stateDot } from "/levels/common.js";
import { MORE, KEYS } from "/goals/shell.js";

const enc = encodeURIComponent;
const TITLES = Object.fromEntries(MORE.map(([k, , t]) => [k, t]));

export function mount(root, ctx) {
  useStyle("g-more", CSS);
  const item = ctx.params.item;
  const el = h("div", { class: "g-more" });
  root.append(el);
  const views = [];
  let alive = true;
  ctx.crumbs([{ label: "Start", href: "#/" }, { label: "More", href: "#/more" }, ...(item ? [{ label: TITLES[item] || item, href: `#/more/${item}` }] : [])]);

  const level = (name, extra) => {
    const host = h("div", { class: "g-lv" });
    el.append(host);
    import(`/levels/${name}.js`).then((m) => { if (alive) views.push(m.mount(host, ctx.levelCtx(extra || {}))); })
      .catch((e) => host.replaceChildren(h("p", { class: "g-bad" }, `This did not load: ${e.message}`)));
  };

  if (!item) {
    el.append(page({ kicker: "More", title: "Everything else", sub: "The four goals are on Start. These are the tools around them." },
      h("nav", { class: "g-mlist" }, MORE.map(([k, ic, t, sub]) =>
        h("a", { class: "more-row", href: `#/more/${k}` }, icon(ic), h("span", {}, t, h("small", {}, sub)), icon("chev", "i sm"))))));
  } else if (item === "time") level("flow");
  else if (item === "library") level("library");
  else if (item === "guide") level("guide");
  else if (item === "rules") rules();
  else if (item === "keys") keys();
  else if (item === "setup") setup();
  else if (item === "settings") settings();

  // ------------------------------------------------------------ rules and grants
  function rules() {
    const p = ctx.project;
    const S = { rules: [], grants: null, busy: new Set(), err: null };
    const rulesHost = h("div");
    const grantsHost = h("div");
    el.append(page({ kicker: p ? p.project : "Rules and grants", title: "Rules and grants", wide: true,
      sub: "Rules are what every agent of the project is told. Grants are what an agent may do without asking, written when you picked a “don't ask again” option." },
    p ? null : h("p", { class: "g-dim" }, "Open a project first: rules and grants belong to one."),
    h("div", { class: "gp-sec" }, pane("Grants", h("span", { class: "g-dim" }, "in each agent's .claude/settings.local.json"), grantsHost)),
    h("div", { class: "gp-sec" }, rulesHost)));
    if (!p) return;
    let panel = null;
    import("/levels/rules.js").then((m) => {
      panel = m.rulesPanel(ctx, {
        repo: () => p.path, rules: () => S.rules, setRules: (l) => { S.rules = l; drawRules(); }, render: () => drawRules(),
        toggle: async (r) => {
          try { const x = await ctx.api(`/api/rules/${enc(r.id)}`, { method: "PATCH", body: { active: !r.active } }); Object.assign(r, x.rule || { active: !r.active }); drawRules(); }
          catch (e) { ctx.toast(`Not changed: ${e.message}`, true); }
        },
        fresh: new Set(), busy: S.busy,
      });
      drawRules();
    });
    const drawRules = () => { if (panel && alive) rulesHost.replaceChildren(panel.view()); };
    ctx.api(`/api/rules?repo=${enc(p.path)}`).then((r) => { S.rules = r.rules || []; drawRules(); }, () => {});
    const loadGrants = () => ctx.api(`/api/grants?repo=${enc(p.path)}`).then((r) => { S.grants = r.grants || []; drawGrants(); }, (e) => { S.err = e.message; drawGrants(); });
    const drawGrants = () => {
      if (!alive) return;
      if (S.err) { grantsHost.replaceChildren(h("p", { class: "g-bad bd" }, S.err)); return; }
      if (!S.grants) { grantsHost.replaceChildren(h("p", { class: "g-dim bd" }, "Reading…")); return; }
      if (!S.grants.length) { grantsHost.replaceChildren(h("p", { class: "g-dim bd" }, "No grants: every agent of this project asks before what Claude Code asks about.")); return; }
      grantsHost.replaceChildren(h("table", { class: "tbl" },
        h("thead", {}, h("tr", {}, h("th", {}, "Rule"), h("th", {}, "Agent"), h("th", { class: "g-hide" }, "File"), h("th", { class: "r" }, ""))),
        h("tbody", {}, S.grants.map((g) => h("tr", {},
          h("td", {}, h("code", {}, g.rule)),
          h("td", {}, g.name || g.agent),
          h("td", { class: "g-hide g-path" }, g.file),
          h("td", { class: "r" }, btn("Revoke", { sm: true, onclick: async (e) => {
            e.target.closest("button").disabled = true;
            try { await ctx.api("/api/grants/revoke", { method: "POST", body: { agent: g.agent, rule: g.rule } }); ctx.toast(`Revoked ${g.rule}: ${g.name || "the agent"} asks again`); loadGrants(); }
            catch (err) { ctx.toast(`Not revoked: ${err.message}`, true); e.target.closest("button").disabled = false; }
          } })))))));
    };
    drawGrants();
    loadGrants();
  }

  // ------------------------------------------------------------ keys
  function keys() {
    el.append(page({ kicker: "More", title: "Keys", sub: "Outside a text box or a terminal. In a terminal every key goes to the agent; Ctrl-b first reaches Workbench." },
      KEYS.map(([t, rows]) => h("div", { class: "gp-sec" }, pane(t, null, h("dl", { class: "kv g-keys" }, rows.map(([k, d]) => [h("dt", {}, k.split(/\s{2}| /).map((x) => kc(x))), h("dd", {}, d)])))))));
  }

  // ------------------------------------------------------------ setup check
  function setup() {
    const body = h("div", { class: "bd" }, h("p", { class: "g-dim" }, "Checking…"));
    const run = () => ctx.api("/api/chloectx").then((s) => draw(s), (e) => draw({ ok: false, problem: e.message }));
    const draw = (s) => {
      if (!alive) return;
      body.replaceChildren(
        s.ok || s.required === false ? stateDot("working", "Ready: ChloeCtx, Claude Code and git are set up.") : stateDot("failed", s.problem || "ChloeCtx is not set up"),
        s.fix ? [h("p", {}, "Run this once, from a checkout of ChloeDB, then check again:"), h("pre", { class: "qc-cmd" }, s.fix)] : null,
        s.version ? h("p", { class: "g-dim" }, `ChloeCtx ${s.version}`) : null,
        h("div", { class: "g-row" }, btn("Check again", { onclick: () => { body.firstChild.replaceWith(h("p", { class: "g-dim" }, "Checking…")); run(); } })));
    };
    el.append(page({ kicker: "More", title: "Setup check", sub: "Workbench gives each session it starts ChloeCtx's hooks and memory tools. Without ChloeCtx, agents do not start." },
      h("div", { class: "gp-sec" }, pane("ChloeCtx, Claude Code and git", null, body))));
    run();
  }

  // ------------------------------------------------------------ settings
  function settings() {
    const tick = (key, title, text) => h("label", { class: "g-set" },
      h("input", { type: "checkbox", checked: ctx.prefs.get(key) ? true : null, onchange: (e) => { ctx.prefs.set(key, e.target.checked); ctx.toast(e.target.checked ? `${title}: on` : `${title}: off`); } }),
      h("span", {}, h("b", {}, title), text));
    el.append(page({ kicker: "More", title: "Settings", sub: "Kept in this browser." },
      h("div", { class: "gp-sec" }, pane("This browser", null, h("div", { class: "bd g-sets" },
        tick("openOnInbox", "Open on the inbox", " Workbench opens on Needs you, every project's, instead of Start."),
        tick("compact", "Compact rows", h("span", {}, " One line per agent on Continue and Start a change. ", kc("z"), " turns it on or off.")))))));
  }

  return {
    update(state) { for (const v of views) { try { v && v.update && v.update(state); } catch (e) { console.error(e); } } },
    unmount() { alive = false; for (const v of views) { try { v && v.unmount && v.unmount(); } catch (e) { console.error(e); } } el.remove(); },
  };
}

const CSS = `
.g-mlist{display:flex;flex-direction:column;border:var(--hair) solid var(--line);border-radius:var(--r);background:var(--surface);overflow:hidden}
.g-mlist .more-row{border-bottom:var(--hair) solid var(--line)}
.g-mlist .more-row:last-child{border-bottom:0}
.g-mlist .more-row>svg:last-child{margin-left:auto;color:var(--text-3)}
.g-lv{min-height:100%}
.g-keys{grid-template-columns:140px minmax(0,1fr);padding:var(--s-3) var(--s-4)}
.g-keys dt{display:flex;gap:4px;align-items:center;text-transform:none;letter-spacing:0}
.g-sets{display:flex;flex-direction:column;gap:var(--s-3)}
.g-set{display:flex;gap:var(--s-3);align-items:flex-start;color:var(--text-2)}
.g-set b{color:var(--text);font-weight:var(--w-mid)}
.g-row{display:flex;gap:var(--s-2);margin-top:var(--s-3)}
.g-more .g-path{font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2);overflow-wrap:anywhere}
.g-more .g-dim{color:var(--text-2)}
.g-more .g-bad{color:var(--crit-ink)}
.g-more .bd{padding:var(--s-3) var(--s-4)}
@media (max-width:640px){.g-more .g-hide{display:none}.g-keys{grid-template-columns:minmax(0,1fr)}}
`;
