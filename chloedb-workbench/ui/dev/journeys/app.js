// The journeys mock's router (JOURNEYS.md, "URL routes"). Pages are plain
// functions page(root, ctx, ...params); reused level modules are mounted
// with ctx.mountModule(name, host, params) on the in-page fixtures.
//   ?theme=dark|light   ?first=1 (the first run)   ?open=needs|agents|search|more
import { h, setFleet } from "../../levels/common.js";
import { api, agents, REPO } from "./fixtures.js";
import { mountShell, setCrumbs, setFirstRun, openOverlay } from "./shell.js";
import { home, firstRun } from "./home.js";
import { where, what, plan } from "./newproject.js";
import { open, understand, change } from "./project.js";
import { continuePage } from "./continue.js";
import { agentPage } from "./agent.js";
import { reviewList, reviewOne } from "./review.js";
import { pick, knew, wrong, code, failingPage } from "./debug.js";
import { page } from "./ui.js";
import { denseShip, denseDiff, denseSession, denseTests, denseContinue, denseRules } from "./dense.js";
import "./phone.js"; // last: the phone and tablet overrides (JOURNEYS.md, "Phone")

const q = new URLSearchParams(location.search);
if (q.get("theme")) document.documentElement.dataset.theme = q.get("theme");
const FIRST = q.get("first") === "1";
setFirstRun(FIRST);

const app = document.getElementById("app");
const scroll = h("main", { class: "jp-scroll" });
app.append(scroll);
mountShell(app);
setFleet(agents);

let views = [];
const OLD = [ // ctx.go from a reused module: today's routes, mapped to the new ones
  [/^#\/story\/(.+)$/, (m) => `#/a/${m[1]}`], [/^#\/ship\/(.+)$/, (m) => `#/p/shop/ship/${m[1]}`], [/^#\/map/, () => "#/p/shop/understand"],
  [/^#\/trace/, () => "#/debug/code"], [/^#\/health/, () => "#/debug/failing"], [/^#\/preview\/(.+)$/, (m) => `#/a/${m[1]}/preview`],
];
function toast(text) { const t = h("div", { class: "jtoast" }, text); document.body.append(t); setTimeout(() => t.remove(), 2200); }

const ctx = {
  crumbs: setCrumbs,
  async mountModule(name, host, params) {
    const mod = await import(`../../levels/${name}.js`);
    const c = {
      api, state: { agents, events: [] },
      params: { repo: REPO, ...params },
      go: (hash) => { for (const [re, to] of OLD) { const m = hash.match(re); if (m) { location.hash = to(m); return; } } toast(`(mock) would open ${hash}`); },
      openWhy: (a) => toast(`(mock) the Why card of ${a.path}:${a.line}`),
      openRewind: () => toast("(mock) Rewind"),
      zoom: (id) => { location.hash = `#/a/${id}/terminal`; },
    };
    if (!host.isConnected) return;
    views.push(mod.mount(host, c));
  },
};

function more(root, ctx, what) {
  const names = { terminals: "All terminals", time: "Where the time went", decisions: "Decisions timeline", library: "Library docs", rules: "Rules", guide: "Guide", keys: "Keys", setup: "Setup check" };
  ctx.crumbs([{ label: "Home", href: "#/" }, { label: "More", href: "#/" }, { label: names[what] || what, href: `#/more/${what}` }]);
  root.append(page({ title: names[what] || what, sub: "One of the expert views, kept as it is today under More. Nothing in the journeys depends on it." }));
}

const ROUTES = [
  [/^#?\/?$/, (r) => (FIRST ? firstRun : home)(r, ctx)],
  [/^#\/new$/, (r) => where(r, ctx)],
  [/^#\/new\/what$/, (r) => what(r, ctx)],
  [/^#\/new\/plan$/, (r) => plan(r, ctx)],
  [/^#\/p$/, (r) => open(r, ctx)],
  [/^#\/(?:p\/shop|catchup)$/, (r) => continuePage(r, ctx)],
  [/^#\/p\/shop\/understand$/, (r) => understand(r, ctx)],
  [/^#\/p\/shop\/change$/, (r) => change(r, ctx)],
  [/^#\/p\/shop\/ship$/, (r) => reviewList(r, ctx)],
  [/^#\/p\/shop\/ship\/([^/]+)$/, (r, m) => reviewOne(r, ctx, m[1])],
  [/^#\/a\/([^/]+)(?:\/(changes|preview|terminal))?$/, (r, m) => agentPage(r, ctx, m[1], m[2] || "progress")],
  [/^#\/(?:debug|why)$/, (r) => pick(r, ctx)],
  [/^#\/debug\/code/, (r) => code(r, ctx)],
  [/^#\/why\?/, (r) => code(r, ctx)],
  [/^#\/debug\/failing/, (r) => failingPage(r, ctx)],
  [/^#\/debug\/[^/]+\/\d+$/, (r) => knew(r, ctx)],
  [/^#\/debug\/[^/]+\/\d+\/wrong$/, (r) => wrong(r, ctx)],
  [/^#\/dense(?:\/ship)?$/, (r) => denseShip(r, ctx)],
  [/^#\/dense\/diff$/, (r) => denseDiff(r, ctx)],
  [/^#\/dense\/session$/, (r) => denseSession(r, ctx)],
  [/^#\/dense\/tests$/, (r) => denseTests(r, ctx)],
  [/^#\/dense\/continue$/, (r) => denseContinue(r, ctx)],
  [/^#\/dense\/rules$/, (r) => denseRules(r, ctx)],
  [/^#\/more\/(\w+)$/, (r, m) => more(r, ctx, m[1])],
];

function route() {
  for (const v of views) { try { v && v.unmount && v.unmount(); } catch (e) { console.error(e); } }
  views = [];
  scroll.replaceChildren();
  scroll.scrollTop = 0;
  const hash = location.hash || "#/";
  for (const [re, fn] of ROUTES) { const m = hash.match(re); if (m) { fn(scroll, m); return; } }
  home(scroll, ctx);
}
window.addEventListener("hashchange", route);
route();
if (q.get("open")) setTimeout(() => openOverlay(q.get("open")), 50);
// ?at=<selector> scrolls that element to the top (for screenshots below the fold).
if (q.get("at")) setTimeout(() => { const e = document.querySelector(q.get("at")); if (e) e.scrollIntoView({ block: "start" }); }, 400);
