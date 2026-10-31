// The views, in the order the navigation shows them.

import overview from "./overview.js";
import topology from "./topology.js";
import performance from "./performance.js";
import alerts from "./alerts.js";
import queries from "./queries.js";
import logs from "./logs.js";
import access from "./access.js";
import system from "./system.js";
import classic from "./classic.js";

export const GROUPS = ["Monitor", "Workload", "Admin", "More"];
export const VIEWS = [overview, topology, performance, alerts, queries, logs, access, system, classic];
