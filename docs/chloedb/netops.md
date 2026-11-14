# chloedb-netops: network operations on ChloeDB

> **A demo.** chloedb-netops shows what ChloeDB can do; it is not part of ChloeDB's releases or their promises. It is tested on a train of its own (the `Demos` workflow, `.github/workflows/demos.yml`), outside CI's required check, so a fault in it does not hold a ChloeDB release, and the command center marks it as a demo ([compatibility](../compatibility.md#demos)).

`chloedb-netops` is a network operations center kept in ChloeDB: the inventory, the topology, a point a minute of telemetry, device logs, alerts, incidents, changes, configurations, runbooks, and agents that work an incident and say what they looked at. It comes with a simulated carrier network, *Meridian* (10 sites, 64 devices, 120 links, 12 services), and a simulator that writes each minute of it and breaks it in six ways, so the whole app can be used, and its agents checked, without a network of your own.

It is there to show what one database does for an operations team when search, SQL, graphs and time all live in it:

- **An incident is a query, not a pile of alerts.** Alert rules are SQL over the telemetry tables. The alerts they raise are grouped into one incident by walking the topology (`REACHABLE` over the links table), so a fibre cut is one incident with eight alerts, not eight pages.
- **The root-cause agent shows its work.** It ranks the alerting devices over the graph (personalised PageRank), reads the logs, changes, optics and flows around them, tests six causes, and names one with a confidence and the evidence for it. Every step keeps the SQL it ran, the rows it read and the time it took. What it packed into its context, and what it left out, is kept as a receipt (`PACK … TOKENS WITH RECEIPT`).
- **Nothing runs without an approval.** The agent proposes actions (reroute a span, roll a change back, restart a process, open a ticket with the circuit's provider) with their risk and the exact commands. A person approves or rejects each one; autopilot, off by default, lets the low-risk ones through.
- **The topology is found from the logs.** LLDP neighbour and routing-adjacency lines in syslog imply links; the app builds them and compares them with the inventory: links the logs confirm, links nobody documented, and documented links nothing has spoken on.
- **Logs are searched three ways in one place**: by words (BM25), by any substring (`LIKE` answered from an FM-index, for an interface name or half an address), and runbooks and past incidents by meaning (a static embedding model on the server, no API call).
- **Most of a shift is not an incident.** The home page is the shift: a handover written from the tables, and one list of what waits (approvals, unowned alerts, changes due, carrier notices that collide). A box at the top takes whatever a ticket says (an address, a circuit id, a customer, an interface) and answers what it is and how it is doing.
- **Every incident has a room that its people shape.** The room opens with a board of panels chosen for the suspected cause. Anyone on the call adds a panel by asking for it in a sentence, removes what is noise, and everyone sees the same board. A panel is one read-only query kept as a row.
- **Engineers and leadership read the same tables.** A switch at the top turns the shift's pages into objectives, incident cost, risk, carriers and a written weekly report, and each number opens the rows behind it.
- **The past is a table.** Every version of each device's configuration is a row, so the change behind an incident is a diff, and "who put this line here" is a query. Which runbook closed which incident is kept as a grade the ranking can learn from.

Like [the crawler](crawler.md) and [the spreadsheets](sheets.md), it is a program of its own, in Rust, that talks to chloedb-server over HTTP only, and the command center shows it as an app.

![My shift: the handover written from the tables, what waits most urgent first, the changes and carrier windows coming up, and notes for the next shift](../assets/img/netops-shift.png)

## Run it

Build both programs, then start chloedb-netops with a server of its own (releases after 0.3.2 carry it prebuilt too: the installer's `--with netops` puts it beside the server, which it then finds without `--server-bin`):

```bash
(cd chloedb-server && cargo build --release)
cd chloedb-netops && cargo build --release
target/release/chloedb-netops serve --local ./netops-data --server-bin ../chloedb-server/target/release/chloedb-server
```

Open `http://127.0.0.1:7700` and go to **Apps → Network operations**. The first start makes the tables and writes six hours of history (`--history-hours`), which takes some seconds; the page shows its progress. From then on the simulator writes a minute of telemetry every minute, evaluates the alert rules, and injects a fault every 45 minutes (`--fault-every`; `0`: only when you press **Inject a fault**).

`--local DIR` starts a chloedb-server on `127.0.0.1:7700` (`--local-listen`) with its database in `DIR`, lists chloedb-netops on it as the app `netops`, and stops it when chloedb-netops stops; the same `DIR` again is the same network, with its incidents.

Against a server or a cluster that runs elsewhere, give chloedb-netops the server (a coordinator or one server) and a key, and give the server the app:

```bash
chloedb-netops serve --strata http://coordinator:7700 --key "$CHLOEDB_KEY" --token "$NETOPS_TOKEN" --listen 0.0.0.0:7840
chloedb-server ... --app netops=http://netops-host:7840   # with CHLOEDB_APP_TOKEN=$NETOPS_TOKEN
```

The key needs admin scope on the tables `netops_*` (a key [limited to tables](server.md#security), `netops_*=admin`, is enough: the app touches no other table). `--token` (or `CHLOEDB_NETOPS_TOKEN`) is the bearer token its API requires; the server sends its `CHLOEDB_APP_TOKEN` ([apps](server.md#apps)). Requests that carry an `Origin` header are refused, so a page on another site cannot reach the app even from inside the network.

On a coordinator over shards the graph steps (`REACHABLE`, `ppr`, `PATH`), which a coordinator does not answer yet, are walked by the app over the same links table; every page says which of the two it used. The app knows a coordinator from its `/v1/health`, or from the first graph statement it refuses, and then sends it none.

`--no-simulate` turns the simulator off: the app reads its network from the inventory tables and writes no telemetry of its own ([your own network](#your-own-network)).

### On Kubernetes

A cluster the [operator](server.md#on-kubernetes) runs gets the app from its `StrataCluster`, with nothing to start by hand:

```bash
kubectl -n strata patch strata demo --type merge -p '{"spec": {"netops": {}}}'
```

The operator runs `chloedb-netops serve` from the cluster's own image as a Deployment of one pod, `<cluster>-netops`, behind a Service of that name (port 7840), and restarts the coordinators so that they list it: **Apps → Network operations** appears in the command center. It holds a key limited to `netops_*` and a token of its own (the Secret `<cluster>-netops-credentials`), or the cluster's admin token until the coordinators make such keys. Its network is rows of the cluster's tables, so a pod started again goes on from them.

| `spec.netops` | default | |
|---|---|---|
| `paused` | `false` | no pod for now (the coordinators still list it, as an app that does not answer; the tables stay and nothing is written to them) |
| `tables` | `netops_*=admin` | the tables its key may touch |
| `args`, `resources` | | more arguments of `chloedb-netops serve` (`--fault-every 0`, `--history-hours 2`, `--no-simulate`), and its container's resources (50m CPU and 96 MiB asked, 1 GiB at most) |

Removing `spec.netops` deletes its Deployment and Service and restarts the coordinators without it; the tables `netops_*` stay.

## The pages

| page | what it is for |
|---|---|
| **My shift** | the home page: a handover of the last twelve hours written from the tables, what waits for you most urgent first (approvals, alerts nobody owns, changes due, carrier notices that collide, open tickets), the changes and carrier windows of the next day and a half, and notes left by the shift before |
| **Diagnose** | [where it is broken](#where-is-it-broken): the stack by place, the lowest broken layer named as where to start, one broken thing investigated five ways, and a trace between two points hop by hop |
| **Network** | the open incident, the network's map with each link's load, traffic and loss, services against their objectives, time to detect and to resolve by cause |
| **Incidents** | every incident; an incident's room has the likely cause with its confidence and evidence, the proposed actions, [its board](#an-incidents-board), a map of where it is that replays it minute by minute, the agent's steps with their SQL, the receipt of what it was shown, the timeline, the blast radius and similar incidents |
| **Alerts** | what fires and who owns it (acknowledge, hand back, silence), the silences in force, and the rules: each is a SQL query you can read, test against the last hours and save |
| **Changes** | the change calendar by day, the collisions (two changes whose devices overlap in a window, `&&` over `INT[]`), the change-risk agent's score, and carrying a change out: start it (its devices are silenced for the window), verify it against the network as it was before, complete it or roll it back |
| **Customers** | every customer with where they attach and whether anything is wrong on their path; a customer's page answers "is it the network?" with what was checked along the path from their port to the backbone, and a reply ready to send |
| **Carriers** | carriers' maintenance notices, each matched to the circuit it names and checked for what it collides with: the other plane's maintenance in the same window, a change on the router that carries the other plane, and whether the surviving plane has the capacity; a notice can be pasted as it arrived |
| **Topology** | the network as the inventory has it, as the logs say it is, and the difference; the path between two devices; the lines of syslog a link was read from |
| **Services** | each service's success rate and latency against its objective, the devices it rides on and the customers on it |
| **Explore** | logs by words or by substring, flows (who talks to whom), and one search box over runbooks, incidents, configurations and tickets |
| **Devices** | the inventory with each device's metrics, what happened on it in the last day, logs, configuration history and blame |
| **Agents** | the six agents, their runs and what each cost in queries, rows and tokens; capacity, advisories and configuration drift; autopilot |
| **Map** | the whole network on one plane, from regions down to one customer's router: zoom in and detail appears, what is broken colours the place that holds it at every zoom, a click on an object says what is wrong and what it has to do with everything around it, and a search box takes a sentence ([below](#the-map)) |
| **Dashboards** | the backbone's busiest links (utilisation, loss, received optical power), the busiest devices and the slowest services over three hours, what is not ok by metro and by kind, faults a day by cause over two weeks, and incidents by cause; the estate's panels follow the place in view on the map |
| **Data** | the 39 tables with their definitions and row counts, where each one's rows come from and whether they are arriving, and the simulator's settings |
| **Leadership** | the switch at the top: services against what was sold, incidents with their cost in customer-minutes and revenue exposed, time to detect and resolve, what caused them and which followed a change, the risk register (spans that cannot fail over at their measured peak, software with open advisories, contracts ending), carriers by cost and downtime, and the week written as a report |

The box above every page searches, and completes as it is typed ([below](#the-box-above-every-page-and-the-assistant)). An id, a site or a circuit opens on the map; a sentence becomes the map's query; anything else is looked up: an address (matched to the customer whose prefix holds it), a circuit id, a customer, a device, an interface, an AS number, with what the thing is and whether it is healthy.

Each page has a **SQL** button that lists the queries behind what it shows, with their times.

![An incident's room: the cause the agent named with its evidence, the actions it proposed, and the room's board](../assets/img/netops-incident.png)

## The map

![The whole network: regions and metros, coloured by what is broken inside them](../assets/img/netops-map.png)

The backbone of 64 devices is what a carrier's core is. What it carries is far more: metro hubs with their routers and power plants, street cabinets, cell sites and business parks, and the ONTs, radios and customer routers behind them. The simulator makes that estate too (`--estate N`, about 200,000 objects by default, `0` for none): 18 kinds of object in `netops_estate_nodes`, what feeds what in `netops_estate_edges` (an edge table, upstream to downstream), and two months of faults and changes on every object in `netops_estate_events`. A few dozen faults are kept going, some of them caused by a change a few minutes before. Two alert rules watch it (`estate-outage` and `estate-degraded`, SQL over the two tables like the others): an access node, a hub router or a hub's power plant that is down or degraded with objects behind it raises an alert, grouped by its hub; a down one with 20 or more behind it is major and opens an incident, which the estate's agent works (what it reported, how far it reaches and which customers, a change before it, who is on call) and proposes sending field services. The degraded ones wait on **My shift**, and **Diagnose** lists them under the backbone's layers. Pasting an object's id into the search opens it.

The Map page draws all of it without ever asking for all of it. Places are boxes, one inside the other: a region holds its metros, a metro its core devices and its hubs, a hub its routers, its power plant and its sites, and a site its access node and what is behind it. A box shows a name and a count until there is room for what is inside it, and then it is divided by layer: the backbone in a band along a metro's top, aggregation and power along a hub's, the premises under a site's access node. Shapes say what a thing is: a cell site is a circle with its sectors around the tower, a business park a box with its corners cut, a router a diamond, a power plant or a tower a triangle.

- **Zoom is a query.** Every object has a place on a plane and a level of detail from which it is worth drawing. Regions, metros and hubs are read once; from there on the page asks for the objects and links in its window at its level of detail, two range queries, and draws what comes back.
- **Red is a `GROUP BY`.** The objects that are down or degraded, and a count by place of what is behind them, are two queries every ten seconds; the page adds them up through site, hub, metro and region, with the backbone's open incidents. A place is coloured by how much the broken things inside it carry: a hub's router weighs more than a cabinet, a cabinet more than one home.
- **A click is a walk.** For one object: what it hangs from (`REACHABLE` against the edges) and the nearest of those that is broken, which is where its trouble comes from; what depends on it, counted by kind and state (`REACHABLE` along the edges, grouped); what else is in trouble within three hops; the changes and faults of thirty days on it and on everything around it, with the change made just before the trouble began; and how faults that read like this one ended before (BM25 over the events).

![One object: where its trouble starts, what it hangs from, what depends on it, and the trouble around it](../assets/img/netops-map-object.png)

A click on an object also lights what it hangs from and what depends on it, and fades the rest; the panel beside the map has tabs for the object: **Overview** (the walk above), **Metrics** (a minute's series for the backbone's devices and their links; for the estate, its latest reading and its faults and changes a day over sixty days), **Events**, **Logs** (syslog for the backbone; the estate's events as lines), **Related** (the chain it hangs from, what depends on it by kind, the customers behind it, the trouble around it), **Incidents** and **Config** (its properties, and for the backbone the latest configuration). They are read by `/v1/map/detail`.

### The box above every page, and the assistant

The box has two modes, **Search** and **Ask** (Tab switches them when the box is empty), and a chip that says where it looks: the object picked, the place the map shows, or everywhere (a click on the chip searches everywhere). As it is typed, `/v1/suggest` completes it from the tables, from what was typed before the word, and from where the person is:

| typed | completed with | how |
|---|---|---|
| the start of an id ("swi", "hr1-h3") | objects, the broken ones first, in the place in view | `PREFIX(id, :w)` over the column's dictionary; with `fuzzy => 'auto'` when nothing matches exactly |
| a word of a name ("park 7", "insur", "Lon") | sites, hubs and metros, customers | `ILIKE` from the substring index, at the start of a word |
| the start of a circuit ("LUM-A") | circuits, with their carrier and plane | `PREFIX(circuit, :w)` |
| `kind:`, `state:`, `vendor:`, `in:` | the values in the place in view, with counts | a `GROUP BY` |
| the start of a sentence ("OLTs in London ") | what the map's reader has not been told yet: a state (with how many it would find), what happened, by whom, a reading, a place in the graph | the reader's plan of the words so far, and one `GROUP BY state` |
| a word that asks for the next ("that is not", "changed in", "about", "by") | the words that can follow it | |
| words of an earlier question | questions asked before | BM25 over `netops_searches` |

In Ask mode it offers questions about the object picked, and after a question the ones that usually follow it (after "what is wrong", what changed before it started, who is affected, what to do). The assistant, on the right of every page, answers them from the app's own API: what is wrong and where the trouble starts, the change made before it, what depends on it and the customers behind it, what to do (the change to check, how such faults ended before, runbooks by BM25), how it went before, what depends on it lit on the map, and a watch that reads the object every 30 seconds and says when it changes. Each answer lists the queries it ran. Anything else becomes the map's sentence search. The answers are written from the query results by fixed rules, not by a model; the command center's own Ask (Ctrl+J) is the server's agent over any table.

Each completion is one to four queries of 1 to 3 ms on one server with 170,000 objects (a name matched at a word's start takes up to 70 ms the first time). ChloeDB has no next-word model: the sentence's next words come from the reader's vocabulary, and earlier questions from BM25.

### Asking in a sentence

The search box takes a question such as "everything changed in the last week about a firmware upgrade by m.rossi with latency above 15 ms", or "customer routers in Frankfurt behind hr1-h03-fra that are not ok". It names up to five kinds of condition, and ChloeDB answers them in one statement:

```sql
SELECT id, kind, name, state, latency_ms, ... FROM netops_estate_nodes
WHERE layer != 'container'
  AND latency_ms > :latency_ms                      -- its latest reading
  AND id IN (                                       -- what happened to it
    SELECT node FROM netops_estate_events
    WHERE event = 'change' AND ts >= :since AND author = :author AND MATCH(text, :about))
  AND REACHABLE(netops_estate_edges, 'ag1-fra',     -- where it sits in the graph
        hops => 6, dir => 'out', through => layer != 'core')
ORDER BY latency_ms DESC LIMIT 300
```

| in the sentence | becomes |
|---|---|
| a kind, a vendor, a version, a place, a state ("degraded Nokia OLTs in Frankfurt") | comparisons on keyword columns |
| a customer's name | `ILIKE` from the substring index |
| a reading ("latency above 40 ms", "high latency", "loss over 1%") | a comparison on the object's latest reading, or of two of its columns (`latency_ms > base_ms * 2`) |
| what happened, when, about what, by whom ("changed last week about QoS by a.khan", "went down in the last 3 days because of power", "not changed in the last month") | a subquery over the events table: time, author, cause, and `MATCH` on the words; `NOT IN` for what did not happen |
| where it sits ("behind ag1-fra", "upstream of ...", "within 2 hops of ...") | `REACHABLE` over the edge table |

The same conditions run a second time grouped by place, so the map shows the matches at every zoom: a count on each region, metro, hub and site that holds some, and a ring on each object once it is drawn.

![A sentence read into conditions, the one statement it became, and the matches by place](../assets/img/netops-map-search.png)

A sentence becomes a plan, and only a plan becomes SQL: every value in it is one the estate has or a parameter. Two readers make plans. With `--rewrite-model` (default `anthropic/claude-haiku-5-5`, empty for none) the app asks the server's own [`llm()`](query-language.md#language-models-llm) to rewrite the question, over the row it was saved as in `netops_searches`; the model is told the plan's shape and what it may name, and its answer is checked field by field. When the server has no model (no key, no network), a reader of fixed vocabulary answers instead, and the page says which of them read the sentence. The fixed reader lists the words it took nothing from beside the count, which is then the answer to the rest of the question; an id, a place or a name the estate does not have refuses the question rather than being dropped from it ("ag9-zzz is not an object of the estate", "Did you mean London?"), and a place one letter off is read and said. A customer is matched by its whole name when the question gives it ("Northwind Energy"), and "this week", "this month" and "this year" start on their first day. The model's path is covered by a unit test of the checking, not by a run against a model: this build was only run where the server had none.

Measured on one server in a 4-vCPU cloud container, with 170,337 objects, 173,133 links and about 470,000 events: the estate is written in about 25 seconds; a window is read in 10 to 75 ms depending on how much is in it; the eight queries behind one object take about 17 ms together; the statement above takes 5 to 10 ms, a walk of the 20,000 objects behind one aggregation router with a condition 22 to 30 ms, and "not changed in the last month" about 290 ms.

On a cluster of shards the range queries, the grouping and the subquery run through the coordinator as they are. A coordinator does not walk graphs yet, so the walks are done by the app over the estate it made, and the page says so. The rows a walk reached are then read by key, at most 400 keys a statement, when there are up to 1,600 of them; a larger walk is read as plain queries over the hubs, metros or regions that hold it, 10,000 rows a page, and the app keeps the rows its walk reached. A search counts its matches by place in the app and reads its first 300 in full by key, so no statement names thousands of keys.

## Where is it broken

A list of alerts says that things are wrong, not where the trouble begins. The Diagnose page answers that, and then asks about the broken thing in the ways that only one database holding the topology, the logs, the history and the readings can.

![Diagnose: the stack by place with the lowest broken layer outlined, a trace that marks the broken hop, and the start of an investigation](../assets/img/netops-diagnose.png)

- **The stack, by place.** A grid with a row per layer (power and reachability, optics, links, capacity, routing, device health, services, customers) and a column per site, plus one for the backbone between sites. Each cell is read from the last three minutes of the tables. A fault shows on its own layer and on every layer above it, so the lowest layer with something broken is named as where to start, and what shows above it is marked as likely following from it.
- **Investigate** takes one link, device or service and asks five things, each card saying which of ChloeDB's abilities answered it, with how many queries and in what time:
  - *Graph queries in SQL*: `ppr()` over the links table ranks the devices closest to everything that alerts now, and `REACHABLE()` with array overlap lists the services and customers it takes down.
  - *Substring search*: every log line that names its interface, on any device, by `LIKE '%…%'` answered from the FM-index, so the neighbours' view of it is there too.
  - *History as rows*: the changes whose devices overlap it (`&&` over `INT[]`) and the configuration versions written on it; when there are none, a change is ruled out.
  - *Hybrid search*: past incidents and runbooks like it, by BM25 and an embedding fused with `rrf()` in the `ORDER BY`.
  - *Time series*: its last hour, a point a minute.
- **Trace a path** walks from a customer, a device or a site to another (`PATH()` over the links table) and shows each hop and the link it was reached over with what they read now, marking the hop where it goes wrong.

On the simulated network an investigation is 13 queries; in a 4-vCPU cloud container (one chloedb-server on local disk, the 64-device network with six hours of telemetry) they took about 12 ms together. The page's SQL button lists each.

## An incident's board

Most incidents fall into a few kinds, and the first minutes of each kind look alike; what differs is the question someone needs answered at minute twenty. So an incident's room is not built from nothing, and it is not fixed either.

- **It starts from its cause's template.** When the room opens it has the panels every incident needs (loss on the links involved, error log lines by kind, the customers at its sites). When the root-cause agent names a cause, that cause's panels join: receive power and input errors for a failing optic, link state, the surviving plane's load and latency for a fibre cut, BGP sessions and configuration versions for a bad change, traffic in and top talkers for a DDoS, CPU and BGP updates for a software defect, reachability and temperature for a power fault.
- **People add to it by asking.** `loss on LON-NYC last 2 hours`, `cpu on cr1-fra`, `logs "LINK-3-UPDOWN"`, `who sends the most`, `customers on ag1-ams`. A small reader that knows the readings, the tables and the network's names (devices, links, circuits, sites) turns the sentence into a query and says what it understood; it refuses what it cannot read rather than guess. A `SELECT` can be pasted instead. No model writes code here.
- **It is shared and kept.** A panel is a row of `netops_panels` holding one read-only query, so everyone in the room sees the same board, a removed panel stays removed, who added what is on the timeline, and the board is still there for the postmortem.
- **It learns in the plainest way.** What people added by hand to other incidents of the same kind is offered, one click each, on the next one.

![The board of a fibre cut: loss, link state, the other plane's load and latency from its template, and panels people asked for](../assets/img/netops-board.png)

![Leadership: services against their objectives, what incidents cost, risk, and the week written as a report](../assets/img/netops-leadership.png)

## The agents

| agent | what it does |
|---|---|
| Triage | sizes a new incident: the alerts, the devices around them, the services and customers that ride on those, and who is on call |
| Root cause | ranks the alerting devices over the topology, reads the logs, changes, optics and flows around them, tests six causes, finds similar incidents and the runbook, and proposes actions |
| Change risk | scores a planned change: its blast radius, other changes on the same or neighbouring devices in its window, open incidents nearby |
| Capacity | finds the links whose 95th percentile runs hot, and the pairs that cannot carry each other's traffic if one fails |
| Advisories and drift | joins vendor advisories with the inventory's software versions, and searches every configuration for what should not be there |
| Topology from logs | reads LLDP and routing-adjacency lines out of syslog, builds the links they imply, and compares them with the inventory |

The agents are code that queries ChloeDB, not calls to a language model: each conclusion comes from rows a query returned, and a hypothesis is kept only when the incident's own alerts and fresh evidence near them support it. That is what makes a run cheap enough to start on every incident, and what lets the page show every step.

The simulator keeps an answer key for each fault it injects (`netops_faults`), which the agents never read. An incident's room can check the agent's answer against it, and the Agents page counts how many were right.

## The six faults

| fault | what happens |
|---|---|
| Fibre cut on a backbone circuit | a backbone circuit loses light; its traffic lands on the other plane, which runs hot |
| A change breaks BGP on a border router | a routing-policy change is committed on a border router and its sessions drop |
| An optic slowly fails | receive power on a link decays, errors climb, and the link starts to flap |
| Volumetric DDoS on a customer prefix | an amplification attack floods a site's borders and its firewalls |
| Routing daemon pegs a core router's CPU | a software defect named in a vendor advisory makes the routing process spin |
| A site loses one power feed | feed A at a small site fails; the devices on it go dark together |

A fault mitigates when its action is approved (or after some minutes on its own) and its incident resolves when the alerts clear.

## Your own network

The app only reads tables, with the SQL its pages show. The simulator is one writer of them; anything that writes to ChloeDB can be another.

`chloedb-netops serve --no-simulate` is the app on a real network:

- **The network is the inventory tables.** At start the app reads `netops_sites`, `netops_devices`, `netops_links`, `netops_services`, `netops_customers` and `netops_bgp` and works from those: the topology graph, an incident's scope, a customer's path, a notice's circuit. With no devices it creates the tables, says which to fill, and stops. The inventory is read at start, so restart the app after it changes.
- **It writes no telemetry.** Each minute, once that minute has had a minute to arrive, the alert rules run over whatever is in `netops_link_metrics`, `netops_device_metrics`, `netops_service_metrics`, `netops_flows` and `netops_syslog`; alerts are grouped into incidents over the topology and the agents work them, exactly as on the simulated network. What the app writes is what it derives: alerts, incidents, timelines, agent runs, proposed actions, panels, silences.
- **An approved action is recorded, not pushed.** The app does not log in to devices. Approval, who gave it and the commands proposed are kept; carrying them out stays with the people and tools that do that today.
- **The Data page says what is arriving.** Each source shows `live` when rows landed in the last five minutes (or, for records, when the table holds any), `silent` or `empty` otherwise, with its rate and lag. With the simulator on, sources read `simulated`.

What usually feeds each table:

| table | from |
|---|---|
| `netops_devices`, `netops_links`, `netops_sites`, `netops_circuits` | the inventory (NetBox, or its database followed with `follow-postgres`) |
| `netops_link_metrics`, `netops_device_metrics`, `netops_service_metrics` | a rollup a minute from the metrics store (Prometheus), one row per link, device or service |
| `netops_syslog` | chloedb-server's [OTLP logs endpoint](server.md), from an OpenTelemetry Collector with a syslog receiver |
| `netops_flows` | flow summaries a minute (top talkers), not raw flows |
| `netops_changes`, `netops_tickets` | the ITSM tool |
| `netops_configs` | a configuration backup tool, a row per version |
| `netops_maintenance` | carriers' mail: `POST /v1/maintenance/parse {"text": "..."}` reads a notice as it arrived into its circuit and window; a row written with only `raw` and `status` `new` is read the same way within a minute |
| `netops_alerts` | the rules here, and `POST /v1/alerts/webhook` in Alertmanager's format (`{"alerts": [{"status": "firing", "labels": {...}, "annotations": {...}}]}`), which join incidents the same way |

A metrics row carries the link's or device's `id` as the inventory has it and a `ts`; the Data page shows each table's columns. The simulator's data is marked as synthetic on every page, and the mark is gone with the simulator off.

## The tables

| table | what it holds |
|---|---|
| `netops_sites` | sites: points of presence and data centres |
| `netops_devices` | devices, as the inventory records them |
| `netops_links` | the inventory's links: an edge table, so graph queries walk the topology |
| `netops_seen_links` | links read from the logs (LLDP neighbours, routing adjacencies), compared with the inventory |
| `netops_circuits` | leased circuits and who provides them |
| `netops_bgp` | BGP sessions with transit providers and peers |
| `netops_services` | services sold, their objectives and the devices they ride on |
| `netops_customers` | customers, where they attach and what they are known by (prefix, port, circuit) |
| `netops_link_metrics` | a point a minute per link: the rollup an incident needs, not a metrics store |
| `netops_device_metrics` | a point a minute per device |
| `netops_service_metrics` | a point a minute per service: requests, errors, latency |
| `netops_flows` | flow summaries a minute: who talks to whom |
| `netops_syslog` | device logs, searched by words (BM25) and by any substring (an FM-index) |
| `netops_alert_rules` | alert rules: each one is a SQL query over these tables |
| `netops_alerts` | alerts the rules raised |
| `netops_incidents` | incidents: alerts correlated over the topology, with the cause the agent named |
| `netops_timeline` | what happened in each incident, in order |
| `netops_changes` | changes, planned and done, with the devices each touches (`INT[]`, so scopes overlap with `&&`) |
| `netops_configs` | every version of each device's configuration, for blame and search |
| `netops_tickets` | tickets, open and closed |
| `netops_runbooks` | runbooks, found by words and by meaning |
| `netops_judgments` | which runbook closed which incident: grades the ranking can learn from |
| `netops_oncall` | who is on call, by team and shift |
| `netops_advisories` | vendor advisories, joined with the inventory's software versions |
| `netops_agent_runs` | each run of an agent |
| `netops_agent_steps` | every step of every run, with the SQL it ran |
| `netops_receipts` | what each agent was shown and what was left out |
| `netops_actions` | actions agents proposed, and who decided them |
| `netops_silences` | what is kept from paging, by whom, why and until when |
| `netops_maintenance` | carriers' maintenance notices, each matched to a circuit and a window |
| `netops_handovers` | notes left from one shift to the next |
| `netops_panels` | each incident's board: one read-only query a panel |
| `netops_estate_nodes` | everything the network is made of, from regions down to a customer's router: its place on the map, its level of detail, its state and its latest readings |
| `netops_estate_edges` | what feeds what, upstream to downstream (an edge table) |
| `netops_estate_events` | faults, their clearing and changes on every object, with their words, cause and author |
| `netops_searches` | the questions asked in a sentence: who read each, the plan and the SQL it became |
| `netops_faults` | the simulator's answer key (agents never read it) |
| `netops_sources` | where each table's rows come from |
| `netops_state` | the app's own bookkeeping |

The Data page shows each table's `CREATE TABLE`, and they can be queried in the SQL workbench like any other:

```sql
-- the links that lost the most packets since a given minute, worst first
SELECT link, MAX(loss) AS worst FROM netops_link_metrics
WHERE ts >= '2026-11-13T03:00:00Z' GROUP BY link ORDER BY worst DESC LIMIT 5;
```

## What it is not

Out of the box it is a demonstration on synthetic data, and it has been run on real feeds only in its own test (readings written to the server from outside, with the simulator off). The telemetry is a point a minute per link, device and service, which is what an incident needs; ChloeDB is not a metrics store, and a real deployment keeps its high-resolution metrics where they are and writes rollups here. Approving an action changes the simulated network, or is only recorded with the simulator off: nothing is ever sent to a device. The reader behind "add a panel" knows a fixed set of readings and tables; it is not a language model. The inventory is read once at start, and at most 10,000 rows a table.

## Test it

```bash
cd chloedb-netops && cargo test --release
```

`tests/cluster.rs` starts the app on a real chloedb-server (built in `../chloedb-server/target`, or `CHLOEDB_SERVER_BIN`) and checks the tables, the topology found from logs, each of the six faults against its answer key, an approval, the webhook, where it is broken during a fibre cut (the stack by place, an investigation, a trace), the shift's pages (handover, lookup, a customer's check, acknowledging and silencing, a change carried out and verified, carriers' notices and their collisions), an incident's board, the leadership numbers, a restart on the same folder, and then the simulator off: readings written to the server from outside open an incident that the agent diagnoses.
