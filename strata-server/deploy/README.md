# strata-server on Kubernetes

A Docker image and Kubernetes manifests for running Strata as shards behind
a coordinator, and a script that runs them on a local multi-node cluster
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
| `kind-test.sh` | creates the kind cluster and runs each step below |

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
which every server then holds:

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
