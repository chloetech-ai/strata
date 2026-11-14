// The harness mock's router (HARNESS.md, "Routes"). Each page returns
// {crumb, content, insp, mdock?, tab?}; the router puts them into the
// style mock's frame: the toolbar, content beside the inspector, and on a
// phone the dock of actions over the tab bar.
//   ?t=light|dark (or ?theme=)   ?at=<selector> scrolls that element to the top
import { toolbar, tabbar } from "./dom.js";
import { overview, approve, plugs } from "./overview.js";
import { run, results } from "./run.js";
import { permissions, hooks, memory, tools } from "./editor.js";
import { sees } from "./sees.js";
import { evals } from "./evals.js";
import { versionsPage } from "./versions.js";

const q = new URLSearchParams(location.search);
const theme = q.get("t") || q.get("theme");
if (theme) document.documentElement.dataset.theme = theme;

const ROUTES = [
  [/^#\/harness\/?$/, overview],
  [/^#\/harness\/approve$/, approve],
  [/^#\/harness\/run\/\d+$/, run],
  [/^#\/harness\/run\/\d+\/results$/, results],
  [/^#\/harness\/edit\/permissions$/, permissions],
  [/^#\/harness\/edit\/hooks$/, hooks],
  [/^#\/harness\/edit\/memory$/, memory],
  [/^#\/harness\/edit\/tools$/, tools],
  [/^#\/harness\/sees$/, sees],
  [/^#\/harness\/evals$/, evals],
  [/^#\/harness\/versions$/, versionsPage],
  [/^#?\/?$/, plugs],
];

const app = document.getElementById("app");

function toast(text) {
  const t = document.createElement("div");
  t.className = "toast";
  t.textContent = text;
  document.body.append(t);
  setTimeout(() => t.remove(), 2200);
}

function route() {
  const hash = location.hash || "#/";
  const hit = ROUTES.find(([re]) => re.test(hash));
  const p = (hit ? hit[1] : overview)();
  app.innerHTML = `${toolbar(p.crumb)}
    <main class="frame" data-page="${hash}">
      <div class="content">${p.content}</div>
      <aside class="insp">${p.insp}</aside>
    </main>
    ${p.mdock ? `<div class="mdock phone-only">${p.mdock}</div>` : ""}
    ${tabbar(p.tab || "")}`;
  window.scrollTo(0, 0);
  if (q.get("at")) setTimeout(() => { const e = document.querySelector(q.get("at")); if (e) window.scrollTo(0, e.getBoundingClientRect().top + scrollY - 60); }, 300);
}

// The few things a click does in a mock: segmented controls, ticks, autocomplete picks, a toast for the rest.
document.addEventListener("click", (e) => {
  const a = e.target.closest("[data-act], .fix, .cks label, .aci");
  if (!a) return;
  if (a.matches(".fix, .cks label")) { a.classList.toggle("on"); return; }
  e.preventDefault();
  if (a.matches(".aci")) { a.parentElement.querySelectorAll(".aci").forEach((x) => x.classList.toggle("on", x === a)); return; }
  const act = a.dataset.act;
  if (act === "mode" || act === "view") { a.parentElement.querySelectorAll("a").forEach((x) => x.classList.toggle("on", x === a)); if (act === "mode") toast(a.dataset.k === "suggest" ? "Suggest only: nothing changes until you keep it" : a.dataset.k === "auto" ? "Auto within limits: narrowing changes that win are applied; widening ones still wait for you" : "Nightly: the day's sessions are tested at 02:00, within the budget"); return; }
  if (act === "approve") { a.closest(".q").classList.add("done"); toast("Kept in 1.5"); return; }
  if (act === "dismiss") { a.closest(".q").classList.add("gone"); return; }
  toast("(mock) " + (a.textContent || "").trim());
});

window.addEventListener("hashchange", route);
route();
