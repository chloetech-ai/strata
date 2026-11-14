// Whose a table is. A cluster that runs apps holds their machinery beside
// the user's own data (the live cluster had 44 of 50 tables of apps), so
// every list of tables shows "Your tables" first and folds the others by
// the app that keeps them. Known by name: the apps' tables are prefixed.

import { h, store } from "./core.js";

/** [pattern, owner, what its tables hold, machinery?]. Machinery is folded away; an app's own data (an app built in the app creator) is not machinery but is still the app's. */
export const OWNERS = [
  [/^sa_/, "App engine", "the app creator's own records: apps, versions, requests, traces", true],
  [/^app_/, "Apps", "the data of apps built in the app creator", false],
  [/^ctx_|^chloectx_/, "ChloeCtx", "ChloeCtx's memory spaces", true],
  // LIV-12: the network operations app's inventory, telemetry, syslog and incidents: its data, not machinery (its syslog is a source for Logs).
  [/^netops_/, "Network operations", "the network operations app's inventory, telemetry, syslog, alerts and incidents", false],
  [/^fs_/, "Feature store", "the feature store's features, snapshots, models and impressions", false],
  [/^sheets_/, "Sheets", "the spreadsheet app's workbooks and cells", true],
  [/^crawl_/, "Crawler", "the crawler's frontier, pages and crawls", true],
  [/^swarm_/, "Swarm", "simulated users' runs, sessions and findings", true],
  [/^strata_sim$|^chloedb_sim$/, "Simulator", "the traffic simulator's data", true],
  [/^chloedb_connector/, "Connectors", "each connector's config, runs and cursor (the data goes to its target table)", true],
  [/^cc_/, "Command center", "feedback sent from the command center", true],
  [/^_/, "System", "the database's own tables", true],
];

/** The owner of a table, or null for the user's own: {owner, about, machinery}. */
export function ownerOf(name) {
  const n = String(name || "");
  for (const [re, owner, about, machinery] of OWNERS) if (re.test(n)) return { owner, about, machinery };
  return null;
}

/** Whether a table is the user's own (no app keeps it). */
export const isMine = (name) => !ownerOf(name);

/**
 * A list of tables split by owner: {mine, apps: [[owner, about, items]]},
 * the user's own first and the apps' in OWNERS's order. `nameOf` reads an
 * item's name.
 */
export function byOwner(list, nameOf = (x) => (typeof x === "string" ? x : x.name || x.table)) {
  const mine = [], groups = new Map();
  for (const x of list || []) {
    const o = ownerOf(nameOf(x));
    if (!o) { mine.push(x); continue; }
    if (!groups.has(o.owner)) groups.set(o.owner, { owner: o.owner, about: o.about, machinery: o.machinery, items: [] });
    groups.get(o.owner).items.push(x);
  }
  const order = OWNERS.map((o) => o[1]);
  const apps = [...groups.values()].sort((a, b) => order.indexOf(a.owner) - order.indexOf(b.owner));
  return { mine, apps };
}

/** "44 tables of apps: App engine 19, Apps 17, ChloeCtx 5…" */
export function appsLine(apps) {
  const n = apps.reduce((a, g) => a + g.items.length, 0);
  return `${n} ${n === 1 ? "table" : "tables"} of apps: ${apps.map((g) => `${g.owner} ${g.items.length}`).join(", ")}`;
}

/**
 * A list of tables drawn mine first, the apps' folded below under one
 * toggle (open or shut, kept per list as `key`): `draw(item)` draws one.
 * A filter that matches only apps' tables opens the fold (`open`).
 */
export function folded(list, draw, { key = "tables", nameOf, open = false, label = "Tables of apps" } = {}) {
  const { mine, apps } = byOwner(list, nameOf);
  if (!apps.length) return mine.map(draw);
  const k = `owners.open.${key}`;
  const det = h("details", { class: "owners-fold", open: open || store.get(k, false) || !mine.length },
    h("summary", { title: appsLine(apps) }, h("span", { class: "grow" }, label), h("span", { class: "small muted" }, apps.reduce((a, g) => a + g.items.length, 0))),
    apps.map((g) => [h("div", { class: "owners-h small muted", title: g.about }, g.owner, " · ", g.items.length), g.items.map(draw)]));
  det.addEventListener("toggle", () => { if (!open) store.set(k, det.open); });
  return [mine.length ? null : h("div", { class: "small muted", style: { padding: "4px 8px" } }, "No tables of your own yet."), mine.map(draw), det];
}

/** The first of the user's own tables, else the first. */
export const firstMine = (list, nameOf = (x) => x.table || x.name) => (list || []).find((x) => isMine(nameOf(x))) || (list || [])[0];
