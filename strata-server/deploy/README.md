# strata-server on Kubernetes

An operator that runs Strata clusters on Kubernetes and changes them as
their resource says (below), a Docker image, the manifests of a fixed
layout of shards behind a coordinator, and a script that runs them on a
local multi-node cluster
([kind](https://kind.sigs.k8s.io/), Kubernetes in Docker) to test the
distributed behaviour: whether the shards answer as one database would,
what the scatter-gather costs, what happens when a shard or one of its
replicas dies, how to move to more shards, and what network latency does.

| file | what |
|---|---|
| `Dockerfile` | strata-server and strata-server-bench on Debian slim (157 MB); build from the repository root |
| `k8s/shards.yaml` | a set of N shards × R replicas: a headless Service, and per shard a StatefulSet of R pods on different nodes; replica r of shard i serves `/data/<collection>.shard<i>of<N>.strata` (r = 0) or its own copy `...shard<i>of<N>.r<r>.strata` |
| `k8s/coordinator.yaml` | the coordinator, a Deployment pointed at one set of shards (`--shards`, replicas of a shard joined by `\|`) |
| `k8s/single.yaml` | one database holding the whole collection, to compare answers and latency with |
| `k8s/operator/operator.yaml` | the operator: its namespace (`strata-system`), service account, cluster role (which includes making each cluster's Role for its coordinators) and Deployment |
| `k8s/operator/crd.json` | the `StrataCluster` resource definition, which the operator installs when it starts (`strata-server operator --print-crd` writes it) |
| `k8s/operator/cluster.yaml` | a cluster for it to make: 2 shards of 3 copies, 10,000 demo products |
| `kind-test.sh` | creates the kind cluster and runs each step below |

## The operator

`strata-server operator` keeps each `StrataCluster` as its spec says. It
makes a StatefulSet of copies per shard (each copy a pod with a volume of
its own), their headless Service, and the coordinators (a Deployment
behind the Service named as the cluster, port 7700: the API and the
command center), then lays the cluster out and changes it through the
coordinator's admin calls, one step every few seconds, writing what it
found in the resource's status:

```bash
kubectl apply -f strata-server/deploy/k8s/operator/operator.yaml
kubectl apply -f strata-server/deploy/k8s/operator/cluster.yaml
kubectl -n strata get strata -o wide          # NAME SHARDS COPIES PHASE MESSAGE
kubectl -n strata patch strata demo --type merge -p '{"spec": {"shards": 3, "replicas": 2}}'
kubectl -n strata port-forward svc/demo 7700  # http://127.0.0.1:7700
```

| spec | default | |
|---|---|---|
| `shards` | 1 | shards, each holding a part of the 4,096 slots |
| `replicas` | 3 | copies of each shard: a primary and its replicas, on different nodes when there are |
| `coordinators` | 1 | coordinators behind the cluster's Service |
| `schema` or `demo` | | the first table's schema (as `--schema` takes it), or this many demo products split over the shards the cluster starts with |
| `storage` | `{size: 10Gi}` | each copy's volume (`size`, `storageClassName`) |
| `failoverAfterMs` | 3000 | every server's `--failover-after-ms`: a shard elects a new primary on its own when its primary's pod dies |
| `writeAcks` | `all` | every primary's `--write-acks` |
| `tokenSecret` | | a Secret whose key `token` every server and coordinator requires; without it they run with `--no-token`, open to whatever reaches them |
| `clusterTokenSecret` | | a Secret whose key `token` is the cluster's own token (`--cluster-token`), which the servers and coordinators send each other and clients never hold; with it, the `tokenSecret` token goes to the coordinators alone, as the admin token (the operator's too), and may not make the calls between servers. Needs `tokenSecret` |
| `image`, `imagePullPolicy`, `resources`, `coordinatorResources`, `args`, `coordinatorArgs` | `strata-server:dev` | the servers' image, containers and further arguments (`args` for every server, `coordinatorArgs` for every coordinator: `--hedge-ms 50`, `--partial`) |
| `rollout` | `{holdAfterSeconds: 30, soakSeconds: 0}` | how a change is made: `paused`, `gates` (limits on `p99Ms`, `errorRatio`, `replicaBehind`, `diskUsedRatio`: one over its limit for `holdAfterSeconds` holds the change), and `soakSeconds`, how long a restarted copy serves before the next restarts. Not part of a version: changing it restarts nothing |

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
  its primary. Every row stays served and every shard keeps taking writes.
- **New settings for the coordinators** (`coordinatorArgs`,
  `coordinatorResources`, the image): their Deployment rolls them, one
  coordinator at a time.
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
and a rollback that shows its plan first. They go through
`POST /v1/admin/kube/plan` and `POST /v1/admin/kube`, which take
`{"spec": {...}}`, a merge patch of the spec, or `{"rollback": version}`,
checked first, and `POST /v1/admin/kube/resume`.

The operator does not choose primaries: the copies of a shard elect one
among themselves ([failover](../../docs/strata/server.md#failover)), so a
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
strata-server/deploy/kind-test.sh up          # no STRATA_WORK or STRATA_DATA needed for the operator
strata-server/deploy/kind-test.sh image
strata-server/deploy/kind-test.sh operator    # follows the cluster until it is Ready
strata-server/deploy/kind-test.sh scale 3 3   # a third shard
strata-server/deploy/kind-test.sh scale 2 2   # back to two, a copy fewer each
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

## In the cloud

The command center's **Cloud** view (an admin key; see
[the server's docs](../../docs/strata/server.md#in-the-cloud)) plans a cluster
on Amazon EKS or DigitalOcean Kubernetes and writes the files that make it,
with the commands that make the cluster with your own tools. Its **Create
it** button makes the cluster from the server instead, once its name is typed
again: the server asks the cloud for it, installs the operator and the
StrataCluster through the new cluster's API, follows the copies until they
answer, and on **Delete** deletes the StrataCluster and its volumes before
the cluster ([what it needs](../../docs/strata/server.md#making-it-from-the-view)).

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

The rest of this page runs Strata from the manifests under `k8s/` without
the operator, to test what sharding costs.

The databases are files on a volume: the manifests read them from a
`hostPath` at `/data`, which `kind-test.sh up` mounts from the Mac into every
node, so a shard pod finds its file on whichever node it lands. On a real
cluster, replace the `hostPath` with a PersistentVolume per shard.

## The test

From the repository root, with Docker running, `kind` and `kubectl`
installed, and Quora prepared by `strata/scripts/prepare_beir.py`:

```bash
export STRATA_DATA=/path/to/beir STRATA_WORK=/path/outside/the/repo
cd strata-server
# The unsplit database and its splits into 2, 3 and 4 shards (natively; minutes each).
for n in 2 3 4; do cargo run --release --bin strata-server-bench -- \
    --collection quora --work "$STRATA_WORK" --shards $n --reuse --build-only; done
cd ..
strata-server/deploy/kind-test.sh up          # 1 control plane + 3 workers
strata-server/deploy/kind-test.sh image
strata-server/deploy/kind-test.sh single
strata-server/deploy/kind-test.sh shards 2           # 2 shards, 1 replica each (shards 2 2: two each)
strata-server/deploy/kind-test.sh coordinator 2      # coordinator 2 2 for two replicas each
# Time both and compare their answers, from a client pod in the cluster.
strata-server/deploy/kind-test.sh bench two --collection quora --out /data/results \
    --single http://strata-single:7700 --coordinator http://strata-coordinator:7700 --label "2 shards"
```

Then, from the same client, the failure and resharding tests (each a
per-second timeline while one action happens; pin the client with
`BENCH_NODE` to a node you are not about to freeze):

```bash
T=strata-server/deploy/kind-test.sh
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
