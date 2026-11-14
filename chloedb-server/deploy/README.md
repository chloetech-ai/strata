# chloedb-server on Kubernetes

An operator that runs ChloeDB clusters on Kubernetes and changes them as
their resource says (below), a Docker image, the manifests of a fixed
layout of shards behind a coordinator, and a script that runs them on a
local multi-node cluster
([kind](https://kind.sigs.k8s.io/), Kubernetes in Docker) to test the
distributed behaviour: whether the shards answer as one database would,
what the scatter-gather costs, what happens when a shard or one of its
replicas dies, how to move to more shards, and what network latency does.

| file | what |
|---|---|
| `Dockerfile` | chloedb-server, chloedb-server-bench, engine-compare and chloedb-crawl on Debian slim; build from the repository root |
| `k8s/shards.yaml` | a set of N shards × R replicas: a headless Service, and per shard a StatefulSet of R pods on different nodes; replica r of shard i serves `/data/<collection>.shard<i>of<N>.strata` (r = 0) or its own copy `...shard<i>of<N>.r<r>.strata` |
| `k8s/coordinator.yaml` | the coordinator, a Deployment pointed at one set of shards (`--shards`, replicas of a shard joined by `\|`) |
| `k8s/single.yaml` | one database holding the whole collection, to compare answers and latency with |
| `k8s/operator/operator.yaml` | the operator: its namespace (`strata-system`), service account, cluster role (which includes making each cluster's Role for its coordinators) and Deployment |
| `k8s/operator/crd.json` | the `StrataCluster` resource definition, which the operator installs when it starts (`chloedb-server operator --print-crd` writes it) |
| `k8s/operator/cluster.yaml` | a cluster for it to make: 2 shards of 3 copies, 10,000 demo products |
| `kind-test.sh` | creates the kind cluster and runs each step below |

## The operator

`chloedb-server operator` keeps each `StrataCluster` as its spec says. It
makes a StatefulSet of copies per shard (each copy a pod with a volume of
its own), their headless Service, and the coordinators (a Deployment
behind the Service named as the cluster, port 7700: the API and the
command center), then lays the cluster out and changes it through the
coordinator's admin calls, one step every few seconds, writing what it
found in the resource's status:

```bash
kubectl apply -f chloedb-server/deploy/k8s/operator/operator.yaml
kubectl apply -f chloedb-server/deploy/k8s/operator/cluster.yaml
kubectl -n strata get strata -o wide          # NAME SHARDS COPIES PHASE MESSAGE
kubectl -n strata patch strata demo --type merge -p '{"spec": {"shards": 3, "replicas": 2}}'
kubectl -n strata port-forward svc/demo 7700  # http://127.0.0.1:7700
```

| spec | default | |
|---|---|---|
| `shards` | 1 | shards, each holding a part of the 4,096 slots |
| `replicas` | 3 | copies of each shard: a primary and its replicas, on different nodes when there are (preferred, not required: a node without room for a copy, or a cluster of fewer nodes, runs copies of a shard side by side, and Home says so) |
| `coordinators` | 1 | coordinators behind the cluster's Service |
| `schema` or `demo` | | the first table's schema (as `--schema` takes it), or this many demo products split over the shards the cluster starts with |
| `storage` | `{size: 10Gi}` | each copy's volume (`size`, `storageClassName`) |
| `failoverAfterMs` | 3000 | every server's `--failover-after-ms`: a shard elects a new primary on its own when its primary's pod dies |
| `writeAcks` | `all` | every primary's `--write-acks` |
| `tokenSecret` | | a Secret whose key `token` every server and coordinator requires; without it they run with `--no-token`, open to whatever reaches them |
| `clusterTokenSecret` | | a Secret whose key `token` is the cluster's own token (`--cluster-token`), which the servers and coordinators send each other and clients never hold; with it, the `tokenSecret` token goes to the coordinators alone, as the admin token (the operator's too), and may not make the calls between servers. Needs `tokenSecret` |
| `secretEnv` | | environment variables of every server and coordinator taken from Secrets, `[{name, secret, key}]` (`key` defaults to `key`): the keys of models behind an API, `ANTHROPIC_API_KEY` for the agents, LLM fields and reranking, `OPENAI_API_KEY` and the like for hosted embeddings ([model keys](#model-keys)). Changing it restarts the copies one at a time |
| `coordinatorStorage` | | a volume for each coordinator (`size`, default `2Gi`; `storageClassName`), which keeps its history across restarts and rollouts: its log store (`--log-store /data/logs`: events, alert history, query statistics, table growth, which Timeline, Logs, Queries and Tables read), its audit log (`--audit-log /data/audit.jsonl`) and its alert rules and their states (`--alerts /data/alerts.json`); a flag `coordinatorArgs` gives itself is not given again. With it the coordinators run as a StatefulSet `<cluster>-coordinator`, each with a volume of its own, kept when the cluster or a coordinator goes; without it (the default) as a Deployment that keeps all of that in memory only, so that a rollout forgets it. Giving it or taking it away restarts the coordinators: the new ones start beside the old ones, which stop once the new ones serve. A StatefulSet restarts one coordinator before it starts the next, so run 2 `coordinators` or more with it: with one, the cluster's Service has none for the seconds it restarts |
| `requireCopiesApart` | `false` | whether a shard's copies must run on different nodes: the scheduler is required to keep them apart (pod anti-affinity on `kubernetes.io/hostname`) rather than asked to prefer it. A copy then waits, unscheduled, for a node that runs none of its shard's other copies, so the cluster needs at least as many nodes as `replicas`, and one more for a rollout's extra copy of a shard of two (or `rollout.extraCopies: false`). Turning it on or off restarts the copies one at a time, each placed by the new rule. Off, the spread is preferred, so that a cluster of fewer nodes than copies still runs them all |
| `image`, `imagePullPolicy`, `resources`, `coordinatorResources`, `args`, `coordinatorArgs` | `chloedb-server:dev` | the servers' image, containers and further arguments (`args` for every server, `coordinatorArgs` for every coordinator: `--hedge-ms 50`, `--partial`) |
| `rollout` | `{holdAfterSeconds: 30, soakSeconds: 0, onFailure: rollback, progressDeadlineSeconds: 600, verifySeconds: 60, extraCopies: true}` | how a change is made: `paused`, `gates` (limits on `p99Ms`, `errorRatio`, `replicaBehind`, `diskUsedRatio`: one over its limit for `holdAfterSeconds` holds the change), `soakSeconds`, how long a restarted copy serves before the next restarts, `progressDeadlineSeconds`, how long a step of a restart may take, `verifySeconds`, how long the new version must be healthy in a row before it is ready, and `onFailure`, what a version that fails becomes: `rollback` (to the last version that ran everywhere) or `hold`, and `extraCopies`, whether a shard of fewer than three copies takes extra ones while its copies restart. Not part of a version: changing it restarts nothing |
| `upgrade` | `{mode: off, checkMinutes: 10}` | the upgrader: every `checkMinutes` it asks the registry which image the tag `track` names (by default the tag of `image`), and with `mode` `suggest` shows it, with `on` rolls it out, pinned by its digest, once the cluster is ready. Not part of a version ([upgrading](#upgrading-chloedb)) |
| `autoscale` | `{mode: off, cooldownMinutes: 20}` | rules that change `replicas`, `shards` or `storage.size` from how busy and full the servers are: `grow` (`busyAbove`, `forMinutes`, `add`, `max`), `shrink` (`busyBelow`, `forMinutes`, `min`), `volumes` (`fullAbove`, `action`: `grow` or `addShard`), `freeze` (weekly windows in UTC, `from`/`to` like `Fri 16:00`, `allowGrow`) and `cooldownMinutes`; `mode` `on` (the operator acts), `suggest` (a person applies or dismisses) or `off`. Not part of a version: the changes it makes are ([autoscaling](../../docs/chloedb/server.md#autoscaling)) |
| `crawler` | | a crawler beside the cluster: the operator runs `chloedb-crawl serve` (in the same image) as a Deployment of one, `<cluster>-crawler`, with a Service of that name on port 7800, and the coordinators list it as an app, so the command center shows **Apps → Crawler**. It writes, and its API requires, the `tokenSecret` token. `grow` (default true) adds a shard through this resource when a crawl's table holds more than `splitRows` rows on one (default 100,000), up to `maxShards` (8); `paused` keeps it at no pod; `args` and `resources` for its container. Turning it on or off restarts the coordinators, not the servers ([the crawler](../../docs/chloedb/crawler.md#on-kubernetes)) |
| `sheets` | | spreadsheets beside the cluster: the operator runs `chloedb-sheets serve` (in the same image) as a Deployment of one, `<cluster>-sheets`, with a Service of that name on port 7820, and the coordinators list it as an app, so the command center shows **Apps → Sheets**. It reads and writes, and its API requires, the `tokenSecret` token; its workbooks are rows of the tables `sheets_books` and `sheets_cells`. `paused` keeps it at no pod; `args` and `resources` for its container. Turning it on or off restarts the coordinators, not the servers ([the spreadsheets](../../docs/chloedb/sheets.md#on-kubernetes)) |
| `chloectx` | | ChloeCtx beside the cluster: the operator runs `chloectx app` (in the same image) as a Deployment of one, `<cluster>-chloectx`, with a Service of that name on port 7760, and the coordinators list it as an app, so the command center shows **Apps → ChloeCtx** (Context: memory spaces agents write to and ask for context; Seen before: firing alerts beside the past incidents they look like). It reads and writes, and its API requires, the `tokenSecret` token, and calls the model with `secretEnv`; its spaces are the tables `ctx_spaces` and `ctx_<space>`. An init container downloads its embedding model (125 MB, from Hugging Face) at each start; `embedder: hash:<dim>` needs none. `paused` keeps it at no pod; `args` and `resources` for its container. Turning it on or off restarts the coordinators, not the servers ([ChloeCtx on Kubernetes](../../docs/chloectx/app.md#on-kubernetes)) |
| `apps` | | the app engine beside the cluster: the operator runs chloedb-apps from an image of its own (`image`, default `ghcr.io/chloetech-ai/chloedb-apps:latest`) as a Deployment of one, `<cluster>-apps`, with a Service of that name on port 7730 (`serviceType` ClusterIP, NodePort with `nodePort`, or LoadBalancer) and a volume of its own (`storage.size`, default 1Gi, kept when `apps` goes), and the coordinators list it as an app, so the command center shows **Apps → App creator**. It needs `tokenSecret`, keeps its apps in the cluster's tables with that token, calls the model with `secretEnv`'s `ANTHROPIC_API_KEY`, stops calling it once it has spent `spendCapUsd` (default 25 US dollars; 0: no limit), and signs browsers in to its studio at `publicUrl` with links the command center hands out. `publish` (`domain`: your own, with a wildcard DNS record, or `sslip`; `email`) opens the apps their owners publish to everyone at `<name>.<domain>`, behind a load balancer of their own, `<cluster>-apps-public`, and Caddy beside the engine for their certificates. `env` for its `STRATA_APPS_` settings, `paused`, `args` and `resources`. Turning it on or off restarts the coordinators, not the servers ([the app engine on a cluster](../../docs/chloedb/apps.md#on-a-cluster)) |

What a change does:

- **More shards**: a StatefulSet for each new shard; once its first copy
  is ready the shard joins the cluster, then slots move to it until each
  shard holds the same number (give or take one), while the cluster
  serves. **Fewer**: the newest shard is drained (its slots move to the
  others), leaves the cluster, and its StatefulSet and volumes are deleted.
- **More copies**: the StatefulSets grow, and each new pod, started empty
  with the schema, joins its shard as a replica and is sent a copy of the
  primary. **Fewer**: the highest pod of each set leaves its shard first
  (when it is the primary, the operator hands the primary over to a replica
  that answers), then the set shrinks and the pod's volume is deleted.
- A pod that comes back without its volume (its node was replaced) leaves
  its shard and joins again, and is sent a copy.
- **New settings for the servers** (`image`, `imagePullPolicy`, `args`,
  `resources`, `failoverAfterMs`, `writeAcks`, the token Secrets): each
  StatefulSet's pod template takes them at once, but its pods restart only
  when the operator says (`updateStrategy: OnDelete`; each pod carries the
  hash of its settings in the annotation `strata.chloetech.ai/config`).
  The operator restarts one copy at a time, replicas first: a primary is
  first handed to a copy in step, one already restarted when there is one,
  and the next restart waits until every copy is back and caught up with
  its primary. A copy told to stop drains for 5 s first (`STRATA_DRAIN_MS`:
  `/readyz` answers 503 while it goes on serving), so the queries it holds
  finish. Every row stays served, and every shard of three copies or more
  keeps taking writes; a write that meets a handover waits for the new
  primary. Once every server runs the new settings, the version is
  verified ([upgrading](#upgrading-chloedb)).
- **New settings for the coordinators** (`coordinatorArgs`,
  `coordinatorResources`, the image): their Deployment rolls them, one
  coordinator at a time, a new one ready before an old one stops
  (`maxSurge: 1`, `maxUnavailable: 0`) and each old one draining first.
  With `coordinatorStorage` their StatefulSet restarts them one at a time,
  each old one stopping before its new one starts on its volume.
- **A larger `storage.size`**: each copy's volume claim grows in place,
  which its storage class must allow (`allowVolumeExpansion: true`); the
  status says so when it does not. A volume never shrinks (a smaller size
  is refused), and a new `storageClassName` applies to the volumes made
  after it.
- The coordinators' `--shards` list follows the cluster's servers, so that
  a coordinator that starts again finds the cluster; their number follows
  `coordinators`.

Every step but those that heal can be held back:

- **Paused** (`rollout.paused: true`): the operator takes no step toward
  the spec (no copy or shard added or removed, no slot moved, no copy
  restarted; a slot move under way stops once its run ends) until it is
  false again. A copy that comes back without its volume still joins its
  shard again. The phase is `Paused`.
- **Held by a gate**: the operator reads the cluster's live numbers from
  the coordinator before each step: the search p99 and the share of failed
  searches its clients see, the most transactions a replica is behind,
  and the fullest volume. A gate over its limit for `holdAfterSeconds`
  holds the change where it is (`status.rollout.held` says which, at what
  number); nothing done is undone. The hold lasts until it is resumed
  (`POST /v1/admin/kube/resume`, which writes the annotation
  `strata.chloetech.ai/resume` naming the hold, and starts the gates' clocks
  again), its gate's limit changes, or the spec changes.
- **A rollback** (`POST /v1/admin/kube {"rollback": version}`) sets the
  spec back to an earlier version of `status.history`, its `rollout` kept,
  and marks the new version with the annotation
  `strata.chloetech.ai/rollback`: no gate holds it, so that it goes through
  while the version it leaves is unwell.

The status keeps the last 20 versions of the spec (`status.history`: each
one's spec, when it was made, by whom when it came through a coordinator,
when it first ran everywhere, and the version a rollback restores), the
operator's last 50 messages (`status.log`), and the change under way
(`status.rollout`: the gates' clocks, the hold, the copies still on older
settings).

The coordinators run as the service account `<cluster>-coordinator`, which
a Role lets read and patch their own `StrataCluster` and nothing else: the
command center's **Control center** (`kubectl -n strata port-forward
svc/demo 7700`, then `http://127.0.0.1:7700/#/control`) edits the whole
spec as a draft, with the operator's plan for it, and applies it, and the
**Cluster** view shows the operator's phase and message; the operator then
makes the change as above; **Rollout** (`#/rollout`) follows it step by
step with its timeline, pauses and resumes it, sets the gates by dragging
each limit on its own live chart, and lists the versions with their diffs
and a rollback that shows its plan first; **Autoscaling**
(`#/autoscale`) edits `autoscale`, replays it on the servers' last week of
load, and applies or dismisses what it suggests. They go through
`POST /v1/admin/kube/plan` and `POST /v1/admin/kube`, which take
`{"spec": {...}}`, a merge patch of the spec, or `{"rollback": version}`,
checked first, `POST /v1/admin/kube/resume`, and
`POST /v1/admin/autoscale/replay` and `/v1/admin/kube/autoscale/dismiss`.

The operator does not choose primaries: the copies of a shard elect one
among themselves ([failover](../../docs/chloedb/server.md#failover)), so a
dead pod costs a few seconds of writes to its shard whether the operator
runs or not. It takes no step while slots are moving or the cluster map
cannot change, and the status says why (`Creating`, `Scaling`, `Ready`,
`Degraded` with the servers that do not answer, or `Failed` with what is
wrong with the spec). Deleting a `StrataCluster` deletes its pods and keeps
its volumes (`data-<cluster>-s<shard>-<copy>`).

It reaches each cluster's coordinators through their Service, so it runs
in the cluster; `--namespace` limits it to one namespace, and `--no-crd`
leaves the resource definition to you. On kind:

```bash
chloedb-server/deploy/kind-test.sh up          # no CHLOEDB_WORK or CHLOEDB_DATA needed for the operator
chloedb-server/deploy/kind-test.sh image
chloedb-server/deploy/kind-test.sh operator    # follows the cluster until it is Ready
chloedb-server/deploy/kind-test.sh scale 3 3   # a third shard
chloedb-server/deploy/kind-test.sh scale 2 2   # back to two, a copy fewer each
```

Its tests: `tests/operator.rs` runs the same steps with server processes
standing in for pods (a cluster made, grown, healed and shrunk, every row
kept; paused while its slots move; every copy restarted with new settings
one at a time; a rollout paused, held by a gate, resumed and held again,
then let through with each restarted copy serving a second first), and
`tests/operator_kube.rs` makes a cluster's objects, rolls new settings into
a set, grows its volumes, and keeps its versions, resumes a hold and rolls
back on a real API server (`STRATA_ENVTEST_DIR`: a directory with `etcd` and `kube-apiserver`,
as `setup-envtest` downloads them).

### Model keys

What calls a model behind an API reads its key from the environment: the
query console's agent, relevance grading and the on-call agent
(`ANTHROPIC_API_KEY`, on the coordinators), fields made by a language model
from another field's text (on the servers, as they write), reranking by a
language model (on the coordinators), and hosted embeddings
(`OPENAI_API_KEY`, `COHERE_API_KEY`, ..., on both). `secretEnv` gives a
Secret's key to every server and coordinator:

```bash
kubectl -n strata create secret generic anthropic --from-literal=key="$ANTHROPIC_API_KEY"
kubectl -n strata patch strata demo --type merge -p '{"spec": {"secretEnv": [{"name": "ANTHROPIC_API_KEY", "secret": "anthropic"}]}}'
```

Make the Secret first: a copy that starts without it does not start, and
the rollout stops at it. The copies restart one at a time with the
variable, as for any other setting; `GET /v1/config` then says
`"server_key": true`. A variable named `STRATA_...` is refused: those are
the operator's own, and tokens go in `tokenSecret`.

### Upgrading ChloeDB

A new version of ChloeDB is a new image of the servers, rolled out while
the cluster answers queries and takes writes:

```bash
# By hand: name it, as any change of the spec
kubectl -n strata patch strata demo --type merge -p '{"spec": {"image": "ghcr.io/chloetech-ai/chloedb-server:<sha>"}}'
# or through a coordinator: the plan first, then the upgrade (admin scope, audited)
curl -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{"image": "ghcr.io/chloetech-ai/chloedb-server:<sha>", "plan": true}' http://127.0.0.1:7700/v1/admin/kube/upgrade
curl -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{"image": "ghcr.io/chloetech-ai/chloedb-server:<sha>"}' http://127.0.0.1:7700/v1/admin/kube/upgrade
# Or let the upgrader follow the tag: suggest shows a new image, on rolls it out
kubectl -n strata patch strata demo --type merge -p '{"spec": {"image": "ghcr.io/chloetech-ai/chloedb-server:latest", "upgrade": {"mode": "on"}}}'
```

The command center's **Rollout** view (`#/rollout`) shows the same: what
the servers run (by image digest, read from their pods), the newest image
of the tag, the upgrader's mode, and **Upgrade now** with its plan.

The operator then:

1. **Tries the image on every node** that runs copies: a Deployment
   `<cluster>-preflight` of one pod a node runs `chloedb-server --help`.
   An image that cannot be pulled, or does not run there (an architecture
   the image lacks), fails the version before any server restarts. The
   pods are deleted once each one ran, and the nodes keep the image, so
   that no restart waits for a pull.
2. **Restarts the coordinators**, a new one ready before an old one stops.
3. **Adds a third copy to each shard of two** (`rollout.extraCopies`, on
   by default; two to a shard of one): it starts with the new image, is
   sent a copy of the shard's data and catches up before either of the
   shard's own copies restarts, so that a majority of the shard's copies
   serves through each restart. It leaves, its volume deleted, once both
   run the new image. A new image that does not run fails here, before any
   copy of the shard's own restarts. One shard at a time has its extra
   copy, so the nodes need room for one more copy (the Cloud view sizes
   each copy's requests to leave it); an extra copy no node has room for
   waits for room, the status quoting the scheduler, and fails the version
   after `progressDeadlineSeconds`.
4. **Restarts the copies** one at a time, as above.
5. **Verifies the version** for `rollout.verifySeconds`: every copy up, in
   sync and ready, no pod crash-looping or restarting, the gates under
   their limits, and a `SELECT COUNT(*)` of the first table answered
   through the coordinators. The version is ready only then
   (`status.history[].readyMs`).

A version fails when a server cannot run it (its pod crash-loops, cannot
pull its image, or its process exits: the status quotes the pod's last
words), a step takes longer than `progressDeadlineSeconds`, a check of the
verification fails for `holdAfterSeconds`, or a gate is over its limit
for as long. With `onFailure: rollback` the operator sets the spec back to
the last version that ran everywhere, as a rollback (no gate holds it;
`changed-by: operator`), and the history keeps the failed version with
the step it failed in and why (`status.history[].failed`); the servers it
reached restart with the version before, the others never left it. With
`onFailure: hold` nothing more restarts until the change is resumed
(`POST /v1/admin/kube/resume`, "Try it again" in the Rollout view) or the
spec changes. A rollback that fails is held, not rolled back again.

The upgrader asks the registry anonymously (public images, such as
ghcr.io/chloetech-ai/chloedb-server), follows one tag (`upgrade.track`, or
the tag of `image`), and reads the commit each image was built from (its
label `org.opencontainers.image.revision`). An image it rolled out that
failed is kept in `status.upgrade.rejected` and never rolled out again by
the upgrader; `POST /v1/admin/kube/upgrade` refuses it too unless given
`"force": true`.

ChloeDB was called Strata, and its image was
ghcr.io/chloetech-ai/strata-server. For a release, every version is
published under both names as the same image (the same digest), so a
cluster whose `image` has the old name keeps following it. Moving a cluster
to the new name is one change of `image` (and of `upgrade.track`, if it is
set), which restarts each server once, as any upgrade does. The operator's
own resources keep the old name: the `StrataCluster` kind, the
`strata-system` namespace and the `strata-operator` Deployment.

Without the extra copies (`extraCopies: false`), a shard of two copies
serves no reads and takes no writes while one of them restarts: one of
two is not a majority, which a primary needs to serve a read and a write
to be acknowledged. The coordinator holds such a write up to 10 s, and
a read up to 3 s, and answers 503 after that.

A write in flight as its shard's primary is handed over may be answered
409, in doubt: the old primary had it, and the new one may or may not.
The coordinator does not send it again (an `UPDATE ... SET n = n + 1`
applied twice would be wrong); read it back, or send it again when that
does no harm.

`tests/operator.rs` (`a_new_version_rolls_out_under_load_and_a_bad_one_rolls_back`)
proves it on processes: 2 shards of 2 copies and 2 coordinators behind a
Service that sends only to ready ones, four clients searching, looking up
keys and counting rows and one writing every 20 ms while the cluster
upgrades, then gets a version whose servers exit (rolled back to the one
before) and one whose image does not exist (refused before any restart):
no query or write fails, and every write is kept.

## In the cloud

The command center's **Cloud** view (an admin key; see
[the server's docs](../../docs/chloedb/server.md#in-the-cloud)) plans a cluster
on Amazon EKS or DigitalOcean Kubernetes and writes the files that make it,
with the commands that make the cluster with your own tools. Its **Create
it** button makes the cluster from the server instead, once its name is typed
again: the server asks the cloud for it, installs the operator and the
StrataCluster through the new cluster's API, follows the copies until they
answer, and on **Delete** deletes the StrataCluster and its volumes before
the cluster ([what it needs](../../docs/chloedb/server.md#making-it-from-the-view)).

### On AWS

On AWS the view writes what runs the
operator on EKS: a CloudFormation template (a VPC with public subnets in three
zones, so no NAT gateway; the EKS cluster with access entries, the key that
planned it its administrator; a managed node group on Amazon Linux 2023; the
EBS CSI driver through Pod Identity), the operator's manifest, and a
`StrataCluster` sized for the machines picked, with a gp3 storage class. It
reads the machines, their prices and the account's quota from AWS with the
credentials saved in it, has CloudFormation check the template, and creates
nothing: the commands it gives make the cluster with your `aws` and `kubectl`.

```sh
aws cloudformation create-stack --region us-east-1 --stack-name strata-search \
  --template-body file://strata-search.json --capabilities CAPABILITY_IAM
aws cloudformation wait stack-create-complete --region us-east-1 --stack-name strata-search
aws eks update-kubeconfig --region us-east-1 --name strata-search
kubectl apply -f strata-search-operator.yaml
kubectl wait --for condition=established --timeout=180s crd/strataclusters.strata.chloetech.ai
kubectl apply -f strata-search-cluster.yaml
```

To delete it, delete the `StrataCluster` and the claims of its volumes first
(`kubectl -n strata delete pvc --all`): the EBS CSI driver made those
volumes, not the stack, and they stay and cost otherwise. Then delete the
stack.

### On DigitalOcean

On DigitalOcean the view writes the body of DOKS's create request (a node
pool of the size picked, the control plane standard or highly available, no
automatic upgrades), the operator's manifest, and a `StrataCluster` sized for
the size picked, its volumes on DOKS's own `do-block-storage` class. It reads
the sizes, their prices, DOKS's versions and the account's droplet limit with
the token saved in it, and asks DigitalOcean again before it says a plan would
be taken. With `doctl` signed in with a token that may create clusters:

```sh
doctl kubernetes cluster create strata-search --region nyc3 --version 1.37.0-do.2 \
  --ha=false --auto-upgrade=false --node-pool "name=strata;size=m-4vcpu-32gb;count=3;tag=strata" --wait
kubectl apply -f strata-search-operator.yaml
kubectl wait --for condition=established --timeout=180s crd/strataclusters.strata.chloetech.ai
kubectl apply -f strata-search-cluster.yaml
```

`doctl` waits for the nodes and points `kubectl` at the new cluster. To
delete it:

```sh
kubectl delete -f strata-search-cluster.yaml
kubectl -n strata delete pvc --all --wait
doctl kubernetes cluster delete strata-search --dangerous --force
```

`--dangerous` also deletes the volumes and load balancers the cluster made,
which would otherwise stay and cost.

## The fixed layout

The rest of this page runs ChloeDB from the manifests under `k8s/` without
the operator, to test what sharding costs.

The databases are files on a volume: the manifests read them from a
`hostPath` at `/data`, which `kind-test.sh up` mounts from the Mac into every
node, so a shard pod finds its file on whichever node it lands. On a real
cluster, replace the `hostPath` with a PersistentVolume per shard.

## The test

From the repository root, with Docker running, `kind` and `kubectl`
installed, and Quora prepared by `chloedb/scripts/prepare_beir.py`:

```bash
export CHLOEDB_DATA=/path/to/beir CHLOEDB_WORK=/path/outside/the/repo
cd chloedb-server
# The unsplit database and its splits into 2, 3 and 4 shards (natively; minutes each).
for n in 2 3 4; do cargo run --release --bin chloedb-server-bench -- \
    --collection quora --work "$CHLOEDB_WORK" --shards $n --reuse --build-only; done
cd ..
chloedb-server/deploy/kind-test.sh up          # 1 control plane + 3 workers
chloedb-server/deploy/kind-test.sh image
chloedb-server/deploy/kind-test.sh single
chloedb-server/deploy/kind-test.sh shards 2           # 2 shards, 1 replica each (shards 2 2: two each)
chloedb-server/deploy/kind-test.sh coordinator 2      # coordinator 2 2 for two replicas each
# Time both and compare their answers, from a client pod in the cluster.
chloedb-server/deploy/kind-test.sh bench two --collection quora --out /data/results \
    --single http://strata-single:7700 --coordinator http://strata-coordinator:7700 --label "2 shards"
```

Then, from the same client, the failure and resharding tests (each a
per-second timeline while one action happens; pin the client with
`BENCH_NODE` to a node you are not about to freeze):

```bash
T=chloedb-server/deploy/kind-test.sh
Q="--collection quora --out /data/results --concurrency 4"      # bash (word splitting)
C="--coordinator http://strata-coordinator:7700"
BENCH_NODE=strata-worker $T bench crash $Q $C --watch 40 --label "watch crash" &
sleep 15; $T crash strata-shards-2-s1-0; wait       # also: delete POD, pause POD 20
$T shards 3                                         # the new set beside the old
$T bench warm $Q --single http://strata-shards-3-s0-0.strata-shards-3:7700 --watch 20 --label warm   # warm each new shard
$T coordinator 3                                    # move the coordinator (under a --watch run)
$T delay strata-shards-3-s0-0 5                     # +5 ms to every packet sent to that shard; 0 removes it
$T coordinator 2 2 --partial --shard-timeout-ms=300 # answer without a shard that has no replica left
```

The `--watch` output prints one line a second (answers, errors, p50, p99,
the first error). Results of a full run on a Mac are in
[`../results/k8s-mac/README.md`](../results/k8s-mac/README.md).

## Security

These manifests serve a test cluster open: every server runs with
`--no-token`, which a server listening beyond loopback needs when it has no
token. Anything that can reach a pod can read and write. On a real cluster,
put a cluster token in a Secret and give it to every server, coordinators
included (they send it to each other, and only it makes the calls between
servers), give the coordinators an admin token of their own, drop
`--no-token`, and serve TLS. Clients get API keys made on a coordinator,
which every server then holds. The operator does this for a
`StrataCluster` that names both Secrets (`tokenSecret` and
`clusterTokenSecret`, above):

```bash
kubectl -n strata create secret generic strata-cluster-token --from-literal=token="$(openssl rand -hex 32)"
kubectl -n strata create secret generic strata-token --from-literal=token="$(openssl rand -hex 32)"
```

```yaml
          env:
            - name: STRATA_CLUSTER_TOKEN
              valueFrom: {secretKeyRef: {name: strata-cluster-token, key: token}}
            # the coordinators only:
            - name: STRATA_TOKEN
              valueFrom: {secretKeyRef: {name: strata-token, key: token}}
```

With `--tls-cert` and `--tls-key` on the shards (a certificate for their
Service names, from cert-manager say), list them as `https://` in the
coordinator's `--shards` and give it the CA with `--shard-ca`. A
NetworkPolicy that lets only the coordinator reach the shards' gRPC port
is worth adding too.
