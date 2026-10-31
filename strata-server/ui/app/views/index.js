// The views, in the order the navigation shows them.

import overview from "./overview.js";
import topology from "./topology.js";
import performance from "./performance.js";
import alerts from "./alerts.js";
import queries from "./queries.js";
import logs from "./logs.js";
import access from "./access.js";
import system from "./system.js";
import sql from "./sql.js";
import builder from "./builder.js";
import schema from "./schema.js";
import importer from "./import.js";
import relevance from "./relevance.js";
import classic from "./classic.js";

export const GROUPS = ["Monitor", "Workload", "Data", "Tune", "Admin", "More"];
export const VIEWS = [overview, topology, performance, alerts, queries, logs, sql, builder, schema, importer, relevance, access, system, classic];
