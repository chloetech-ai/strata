# strata-server on Kubernetes

A Docker image and Kubernetes manifests for running Strata as shards behind
a coordinator, and a script that runs them on a local multi-node cluster
([kind](https://kind.sigs.k8s.io/), Kubernetes in Docker) to test the
distributed behaviour: whether the shards answer as one database would,
what the scatter-gather costs, what happens when a shard dies, how to move
to more shards, and what network latency does.

| file | what |
|---|---|
| `Dockerfile` | strata-server and strata-server-bench on Debian slim (157 MB); build from the repository root |
| `k8s/shards.yaml` | a set of N shards: a StatefulSet whose pod ordinal picks its file, `/data/<collection>.shard<i>of<N>.strata`, and a headless Service |
| `k8s/coordinator.yaml` | the coordinator, a Deployment pointed at one set of shards |
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
strata-server/deploy/kind-test.sh shards 2
strata-server/deploy/kind-test.sh coordinator 2
# Time both and compare their answers, from a client pod in the cluster.
strata-server/deploy/kind-test.sh bench two --collection quora --out /data/results \
    --single http://strata-single:7700 --coordinator http://strata-coordinator:7700 --label "2 shards"
```
