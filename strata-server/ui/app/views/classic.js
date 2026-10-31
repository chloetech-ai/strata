// The console that came before the command center, kept whole while its
// tools move here: the query builder, the graph explorer, the SQL editor,
// the agent, the schema designer and imports, the query language guide.

import { h } from "../core.js";

export default {
  id: "classic", title: "Query console", icon: "classic", group: "More", full: true,
  keywords: "builder graph sql editor agent ask schema import query language guide old",
  render(page) { page.append(h("iframe", { class: "classic", src: "/classic", title: "Query console" })); },
};
