// Apps: programs of their own that this server forwards to (--app
// crawler=URL), each shown here as a view: #/app/<name>. The server lists
// them (GET /v1/apps) and forwards /v1/apps/<name>/... to the app, with
// its own token in place of the signed-in key.
//
// An app's view is a module it serves (its manifest's `view`), loaded
// through the server and run with the command center's own helpers: its
// default export's render(el, ctx) draws into `el` and may return a
// function that stops it. ctx = { core, charts, data, app, route, api,
// path, href, onPath }, where api(path, opts) calls the app's own API
// (core.api with the /v1/apps/<name> prefix) and core.api calls this
// server's. An app's page has paths of its own: #/app/<name>/a/b is
// path() ["a", "b"], href("a", "b") links to it, and onPath(f) calls f
// when the path changes (the page is not drawn again).

import * as core from "../core.js";
import * as charts from "../charts.js";
import * as data from "../data.js";

const { h, empty, failure, bus } = core;

export default { id: "app", title: "Apps", icon: "box", group: "Apps", range: false, keywords: "apps app plugin crawler sheets spreadsheet", render };

let apps = null;
let asked = null;

/** The server's apps, asked once (again with `again`). */
export function loadApps(again = false) {
  if (!asked || again) {
    asked = core.api("/v1/apps", { timeout: 8000 }).then((j) => {
      const was = JSON.stringify(apps);
      apps = j.apps || []; appsError = null;
      if (JSON.stringify(apps) !== was) bus.emit("apps");
      return apps;
    }).catch((e) => { apps = apps || []; appsError = e; return apps; });
  }
  return asked;
}
// The list is asked again every minute (the server keeps the manifests), so
// the nav learns that an app went down or came back without a reload.
core.every(60000, () => (apps ? loadApps(true) : null), false);
export const listApps = () => apps || [];
// Why the listing failed (a refusal is not "no apps").
let appsError = null;

const ALLOWED = { path: ["d"], circle: ["cx", "cy", "r"], ellipse: ["cx", "cy", "rx", "ry"], rect: ["x", "y", "width", "height", "rx", "ry"], line: ["x1", "y1", "x2", "y2"], polyline: ["points"], polygon: ["points"] };

/** An app's icon: the shapes of its manifest's `icon_svg` (only shapes and their geometry), else a box. */
export function appIcon(a, cls = "") {
  if (!a?.icon_svg) return core.icon("box", cls);
  const svg = document.createElementNS(core.SVGNS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("class", "i " + cls);
  svg.setAttribute("aria-hidden", "true");
  try {
    const doc = new DOMParser().parseFromString(`<svg xmlns="${core.SVGNS}">${a.icon_svg}</svg>`, "image/svg+xml");
    for (const el of doc.documentElement.querySelectorAll("*")) {
      const keep = ALLOWED[el.localName];
      if (!keep) continue;
      const out = document.createElementNS(core.SVGNS, el.localName);
      for (const k of keep) { const v = el.getAttribute(k); if (v != null && /^[-0-9.,\s a-zA-Z]*$/.test(v)) out.setAttribute(k, v); }
      svg.append(out);
    }
  } catch { return core.icon("box", cls); }
  return svg.childNodes.length ? svg : core.icon("box", cls);
}

const modules = new Map();
/** An app's view module: its text read through the server (with the key), imported from a blob. */
function load(a) {
  if (!modules.has(a.name)) {
    modules.set(a.name, (async () => {
      const src = await core.api(a.view, { text: true });
      const url = URL.createObjectURL(new Blob([src], { type: "text/javascript" }));
      try { return await import(url); } finally { URL.revokeObjectURL(url); }
    })().catch((e) => { modules.delete(a.name); throw e; }));
  }
  return modules.get(a.name);
}

/** The path of an app's page under #/app/<name>/. */
function subPath(name) {
  const [path] = location.hash.replace(/^#\/?/, "").split("?");
  const parts = path.split("/").map((x) => { try { return decodeURIComponent(x); } catch { return x; } });
  return parts[0] === "app" && parts[1] === name ? parts.slice(2).filter(Boolean) : [];
}

function render(page, route) {
  const name = route.rest[0];
  let stop = null, gone = false;
  const watching = [];
  const draw = (list) => {
    if (gone) return;
    page.replaceChildren();
    if (!name) {
      page.append(h("div", { class: "page-head" }, h("h1", {}, "Apps")),
        list.length ? h("div", { class: "grid g3" }, list.map((a) => h("a", { class: "card", href: "#/app/" + a.name, style: { padding: "16px", display: "flex", gap: "12px", alignItems: "flex-start" } },
          appIcon(a, "lg"), h("div", {}, h("b", {}, a.title), h("div", { class: "small dim" }, a.up ? a.description || "" : `not answering: ${a.error}`),
            core.limited() && !a.checks_callers ? h("div", { class: "small muted" }, "takes a key of every table") : null))))
          : appsError ? empty("warn", "Can't list the apps", core.cannotSee(appsError, "the apps")) : empty("box", "No apps", "An app is a program of its own that this server forwards to. The server starts the crawler (chloedb-crawl), Sheets (chloedb-sheets) and ChloeCtx (chloectx) itself when they are installed beside it (--start-apps); others it reaches with --app name=url."));
      return;
    }
    const a = list.find((x) => x.name === name);
    if (!a && appsError) { page.append(empty("warn", `Can't list the apps`, core.cannotSee(appsError, "the apps"))); return; }
    if (!a) { page.append(empty("box", `No app ${name}`, list.length ? `This server's apps: ${list.map((x) => x.name).join(", ")}.` : "This server has no apps (--app name=url).")); return; }
    if (!a.up) {
      const p = a.process;
      const how = p ? (p.running ? `This server started it (${p.program}, pid ${p.pid}); it may still be starting.` : `This server starts it (${p.program}): ${p.last_exit || "not running"}${p.restarts ? `, started again ${p.restarts} times` : ""}.`) : `The server reaches it at ${a.url}.`;
      page.append(empty("warn", `${a.title} does not answer`, `${a.error}. ${how}`, h("button", { class: "btn sm", onclick: () => loadApps(true).then(draw) }, core.icon("refresh"), "Ask again")));
      if (p?.tail?.length) page.append(h("pre", { class: "code", style: { marginTop: "12px", whiteSpace: "pre-wrap", maxHeight: "320px", overflow: "auto", fontSize: "12px" } }, p.tail.join("\n")));
      return;
    }
    if (!a.view) { page.append(empty("box", `${a.title} has no view`, "Its manifest names none.")); return; }
    // An app that does not check a key limited to tables itself takes a key of every table.
    if (core.limited() && !a.checks_callers) { page.append(empty("lock", `This key can't open ${a.title}`, `${a.title} acts on the cluster with its own token and does not check a key limited to tables, so it takes a key of every table. This key is limited to ${core.grantsText(data.state.whoami?.tables)}.`)); return; }
    // The view module can take a while: say so until it is there.
    const waiting = empty("box", `Loading ${a.title}…`, "");
    page.append(waiting);
    load(a).then((m) => {
      if (gone) return;
      waiting.remove();
      const view = m.default || m;
      const ctx = {
        core, charts, data, app: a, route,
        api: (p, o) => core.api(`/v1/apps/${a.name}${p}`, o),
        path: () => subPath(a.name),
        href: (...parts) => `#/app/${a.name}` + parts.map((x) => "/" + encodeURIComponent(x)).join(""),
        onPath: (f) => {
          let last = subPath(a.name).join("/");
          const l = () => { const p = subPath(a.name); if (!gone && location.hash.startsWith(`#/app/${a.name}`) && p.join("/") !== last) { last = p.join("/"); f(p); } };
          addEventListener("hashchange", l);
          watching.push(l);
        },
      };
      const r = view.render(page, ctx);
      stop = typeof r === "function" ? r : r && typeof r.stop === "function" ? () => r.stop() : null;
    }).catch((e) => {
      if (gone) return;
      waiting.remove();
      // A failed load is not cached (load forgets it): asking again asks the list too, which says whether the app is up.
      page.append(failure(e, `${a.title}'s view`),
        h("div", { style: { marginTop: "12px" } }, h("button", { class: "btn sm", onclick: () => loadApps(true).then(draw) }, core.icon("refresh"), "Ask again")));
    });
  };
  if (apps) draw(apps); else loadApps().then(draw);
  return () => { gone = true; for (const l of watching) removeEventListener("hashchange", l); try { stop?.(); } catch (e) { console.warn(e); } };
}
