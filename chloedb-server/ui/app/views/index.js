// The views, and the navigation: Build (data in, search, saved searches),
// Watch (is it healthy), Run the cluster (its machines and settings), the
// apps, then Labs (tools for search engineers), collapsed and pinnable.
//
// A view module exports { id, title, group, icon, render(el, route), onParams?,
// badge?() -> { n, tone } | null, keywords?, full? }. NAV fixes the order, the
// names the navigation shows and the shortcuts; a view a module does not
// provide (yet) is left out of the navigation.

import home from "./overview.js";
import timeline from "./timeline.js";
import alerts from "./alerts.js";
import oncall from "./oncall.js";
import cluster from "./cluster.js";
import logs from "./logs.js";
import explorer from "./explorer.js";
import queries from "./queries.js";
import statement from "./statement.js";
import performance from "./performance.js";
import profile from "./profile.js";
import querypath from "./querypath.js";
import tables from "./tables.js";
import inspector from "./inspector.js";
import rollout from "./rollout.js";
import autoscale from "./autoscale.js";
import simulator from "./simulator.js";
import access from "./access.js";
import system from "./system.js";
import cloud from "./cloud.js";
import connections from "./connections.js";
import clusters from "./clusters.js";
import follow from "./follow.js";
import sql from "./sql.js";
import schema from "./schema.js";
import importer from "./import.js";
import slices from "./slices.js";
import models from "./models.js";
import builder from "./builder.js";
import relevance from "./relevance.js";
import autopilot from "./autopilot.js";
import recsys from "./recsys.js";
import sid from "./sid.js";
import autocomplete from "./autocomplete.js";
import understand from "./understand.js";
import media from "./media.js";
import graph from "./graph.js";
import classic from "./classic.js";
import apps from "./apps.js";
import feedback from "./feedback.js";
import start from "./start.js";
import tablehome from "./table.js";
import saved from "./saved.js";
import connectors from "./connectors.js";

export const VIEWS = [home, timeline, alerts, oncall, cluster, logs, explorer, queries, statement, performance, profile, querypath, tables, inspector, rollout, autoscale, simulator, access, system, cloud, connections, clusters, follow, sql, schema, models, importer, slices, builder, relevance, autopilot, recsys, sid, autocomplete, understand, media, graph, classic, apps, feedback, start, tablehome, saved, connectors];

export const GROUPS = ["Build", "Watch", "Run the cluster", "Apps", "Labs"];

/**
 * The navigation, in order: [group, [{id, title, icon, key, scope, cluster}]].
 * `key` is the letter after "g"; `scope` the key scope that changes things
 * there (the entry says so when the signed-in key has less); `cluster`: a
 * page about a cluster's shards and copies, left out of the sidebar on a
 * single server (the palette still finds it). The shell puts Build first
 * on a single server and Watch first on a coordinator.
 */
export const NAV = [
  ["Build", [
    { id: "start", title: "Start here", icon: "rocket", key: "g" },
    { id: "table", title: "Tables", icon: "table", key: "e" },
    { id: "saved", title: "Saved searches", icon: "bookmark", key: "v" },
    { id: "builder", title: "Query builder", icon: "builder", key: "b" },
    { id: "sql", title: "SQL workbench", icon: "sql", key: "w" },
    { id: "import", title: "Import and export", icon: "import", key: "i", scope: "write" },
    { id: "connectors", title: "Connectors", icon: "streams", scope: "admin" },
    { id: "models", title: "Models", icon: "cpu" },
    { id: "schema", title: "Schema", icon: "schema", key: "d" },
    { id: "slices", title: "Slices", icon: "layers" },
  ]],
  ["Watch", [
    { id: "home", title: "Home", icon: "overview", key: "h" },
    { id: "alerts", title: "Alerts", icon: "alerts", key: "a" },
    { id: "logs", title: "Logs", icon: "logs", key: "l" },
    { id: "timeline", title: "Timeline", icon: "timeline", key: "t" },
    { id: "explorer", title: "Log explorer", icon: "search", key: "m" },
    { id: "queries", title: "Queries", icon: "queries", key: "q" },
    { id: "performance", title: "Performance", icon: "perf", key: "p" },
    { id: "oncall", title: "On-call agent", icon: "agent", key: "n", scope: "admin", route: "POST /v1/admin/oncall" },
    { id: "feedback", title: "Feedback", icon: "feedback", key: "f", scope: "write", route: "POST /v1/feedback" },
  ]],
  ["Run the cluster", [
    { id: "cluster", title: "Cluster", icon: "cluster", key: "c" },
    { id: "tables", title: "Tables and storage", icon: "layers", key: "o", fallback: "inspector" },
    { id: "profile", title: "Profiler", icon: "flame", key: "r" },
    { id: "querypath", title: "Query path", icon: "route", key: "x", cluster: true },
    { id: "rollout", title: "Rollouts", icon: "rollout", key: "u", scope: "admin", cluster: true },
    { id: "autoscale", title: "Autoscaling", icon: "autoscale", scope: "admin", cluster: true },
    { id: "simulator", title: "Simulator", icon: "gauge", scope: "admin", cluster: true },
    { id: "access", title: "Access and audit", icon: "access", key: "k", scope: "admin" },
    { id: "system", title: "Settings", icon: "system", key: "s", scope: "admin" },
    { id: "cloud", title: "Cloud setup", icon: "cloud", scope: "admin" },
    { id: "connections", title: "Connections", icon: "key", scope: "admin" },
    { id: "clusters", title: "All clusters", icon: "cluster", scope: "admin" },
    { id: "follow", title: "Follow", icon: "share", scope: "admin", cluster: true },
  ]],
  // Apps (--app name=url): one entry each, added by the shell once the
  // server lists them (views/apps.js); the group shows only when it has one.
  ["Apps", []],
  ["Labs", [
    { id: "relevance", title: "Relevance lab", icon: "relevance" },
    { id: "autopilot", title: "Relevance autopilot", icon: "relevance", scope: "admin" },
    { id: "recsys", title: "Recsys eval", icon: "recsys" },
    { id: "sid", title: "Semantic IDs", icon: "sid" },
    { id: "autocomplete", title: "Autocomplete", icon: "autocomplete" },
    { id: "understand", title: "Understanding", icon: "sparkle" },
    { id: "media", title: "Media search", icon: "media" },
    { id: "graph", title: "Graph explorer", icon: "graph" },
    // CON-1: the old console (#/classic) is out of the nav: its tools are
    // views here; the SQL workbench links it for its agent.
  ]],
];

/**
 * Old addresses and where they go now: [from, to, params mapper?]. A route
 * whose view does not exist (yet) falls back as NAV says (`fallback`).
 */
export const REDIRECTS = {
  overview: "home",
  topology: "cluster",
  fleet: "cluster",
  control: "cluster",
  settings: "system",
  storage: "tables",
  tablehome: "table",
  saved_searches: "saved",
  functions: "saved",
};
