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
        h("a", { class: "more-row", href: `#/more/${k}` }, icon(ic), h("span", {}, t, h("small", {}, sub)), icon("chev", "i sm"))),
        // On a phone the top bar has no room for the bell: Notifications is here.
        h("button", { type: "button", class: "more-row g-nt", onclick: () => ctx.notifications && ctx.notifications() }, icon("bell"), h("span", {}, "Notifications", h("small", {}, "Notify me, quiet hours, a level per project")), icon("chev", "i sm")))));
  } else if (item === "time") level("flow");
  else if (item === "library") level("library");
  else if (item === "guide") level("guide");
  else if (item === "rules") rules();
  else if (item === "keys") keys();
  else if (item === "setup") setup();
  else if (item === "settings") settings();

  // ------------------------------------------------------------ rules and grants
  // Agent P's page (rules-suggest.js: suggested rules, every grant, Revoke),
  // then Decide's rules, what every agent of the project is told.
  function rules() {
    const p = ctx.project;
    const S = { rules: [], busy: new Set() };
    const top = h("div");
    const rulesHost = h("div");
    el.append(top, h("div", { class: "g-told" }, rulesHost));
    import("/goals/rules-suggest.js").then((m) => {
      if (!alive) return;
      views.push(m.mount(top, ctx));
      ctx.crumbs([{ label: "Start", href: "#/" }, { label: "More", href: "#/more" }, { label: TITLES.rules, href: "#/more/rules" }]);
    }).catch((e) => top.replaceChildren(h("p", { class: "g-bad" }, `This did not load: ${e.message}`)));
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
    const drawRules = () => { if (panel && alive) rulesHost.replaceChildren(label("What every agent is told", h("span", { class: "num" }, String(S.rules.length))), panel.view()); };
    ctx.api(`/api/rules?repo=${enc(p.path)}`).then((r) => { S.rules = r.rules || []; drawRules(); }, () => {});
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
      body.replaceChildren(...[
        s.ok || s.required === false ? stateDot("working", "Ready: ChloeCtx, Claude Code and git are set up.") : stateDot("failed", s.problem || "ChloeCtx is not set up"),
        s.fix ? [h("p", {}, "Run this once, from a checkout of ChloeDB, then check again:"), h("pre", { class: "qc-cmd" }, s.fix)] : null,
        s.version ? h("p", { class: "g-dim" }, `ChloeCtx ${s.version}`) : null,
        h("div", { class: "g-row" }, btn("Check again", { onclick: () => { body.firstChild.replaceWith(h("p", { class: "g-dim" }, "Checking…")); run(); } }))].flat().filter((k) => k != null && k !== false));
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
.g-mlist .more-row:nth-last-child(2){border-bottom:0}
.g-mlist .g-nt{display:none;width:100%;border:0;background:none;font:inherit;text-align:left;cursor:pointer}
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
.g-told{max-width:var(--page-w);padding:0 var(--s-7) 64px;margin-top:-32px}
@media (max-width:1024px){.g-told{padding:0 var(--s-5) 48px;margin-top:-24px}}
@media (max-width:640px){.g-mlist .g-nt{display:grid}.g-mlist .more-row:nth-last-child(2){border-bottom:var(--hair) solid var(--line)}.g-told{padding:0 var(--s-4) var(--s-6);margin-top:0}.g-more .g-hide{display:none}.g-keys{grid-template-columns:minmax(0,1fr)}}
`;
