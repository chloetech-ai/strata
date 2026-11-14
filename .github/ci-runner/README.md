# CI's on-demand runner

One DigitalOcean droplet (CPU-Optimized `c-16`: 16 vCPUs, 32 GB, $0.50 an
hour) takes the longest jobs of every CI run that changes the engine or the
server: chloedb-server's release binary (job `server-bin`, which the smoke
test, the Python and JavaScript packages and the guide's pages then
download), ChloeDB's tests and chloedb-server's tests, which set how long a
run takes (35 minutes on GitHub's 4-core runners on 13 November). Every
other job stays on GitHub's runners. With the repository variable
`CI_RUNNER=overflow` the droplet takes them only when GitHub's runners are
full (a Linux job has waited over a minute for one, or fewer than 6 of the
account's 60 are free), as it did until 14 November; `CI_RUNNER=off` turns
it off. The droplet exists only while
it is needed:

| what | where |
|---|---|
| made when a run overflows and no droplet is up, and the run waits up to 4 minutes for it | `ci_runner.py want`, ci.yml's job `runner` |
| destroyed once no CI run has been queued or running for 15 minutes; after 12 hours whatever happens (14 if a job still runs); after 11 minutes if no runner came up | `ci_runner.py reap`, `ci-runner.yml` (after every CI run, and every 15 minutes) |
| starts from the snapshot `ci-runner-base-<time>`: Ubuntu 24.04, Rust (ci.yml's `RUST_TOOLCHAIN`), the Actions runner | `provision.sh`, `ci_runner.py image`, `ci-runner.yml`'s job `image` (by hand, monthly, and when this folder changes on main) |
| keeps cargo's registry, the runners' work folders and the build folders on the volume `ci-runner-cache` (250 GB: each runner keeps its own build folders, about 90 GB; a job starts by removing its runner's when under 60 GB are free), attached to each new droplet | `boot.sh`, `target-dir.sh` |
| keeps a compile cache (sccache, 30 GB per runner, each with its own server) on the volume, so a build folder that starts over recompiles from the cache | `provision.sh`, `target-dir.sh` |
| refuses a job of a pull request from a fork, or of another repository, before its first step | `job-started.sh`, the runner's job-started hook |

The droplet is destroyed rather than powered off because DigitalOcean bills a
powered-off droplet in full. When the droplet is off, busy with another
run's jobs, not set up, or does not come up in time, the jobs run on
`ubuntu-latest` as before, and the run's summary says which and why.

## Switching it on

It needs two secrets in the environment `ci-runner` (Settings,
Environments, `ci-runner`; set its deployment branches to `main` alone, and
keep neither secret at the repository's level, where a branch's copy of
ci.yml could read it):

- `DIGITALOCEAN_TOKEN`: a DigitalOcean API token with write access. A
  custom-scoped token needs droplets, snapshots, block storage (volumes),
  images, tags and actions.
- `CI_RUNNER_ADMIN_TOKEN`: a fine-grained GitHub token for this repository
  only, with the permission **Administration: read and write**: it makes the
  runners' registration tokens and reads whether they are busy.

Only main's code reads them. A run of main (a push, the nightly run, a run
by hand) makes the droplet in ci.yml's job `runner`. Any other run, a pull
request's, the merge queue's or a tag's, has no secret: ci-runner.yml's job
`place`, which GitHub runs from main when the run is requested, makes the
droplet for it once its job `runner` starts, and posts the label as the
commit status `ci-runner`, which that job waits for (2 minutes for the
first status, 7 for the label, then GitHub's runners).

Then run the workflow "CI runner" by hand with `image` once, which builds
the first snapshot (about 15 minutes, a few cents).

Repository variables, all optional: `CI_RUNNER=off` sends every job back to
GitHub's runners; `CI_RUNNER_HOSTED_LIMIT` (default 60, about the jobs GitHub
ran at once for this account on 13 November); `CI_RUNNER_SIZE` (default `c-16`), `CI_RUNNER_REGION`
(`nyc1`, where `c-16` is offered; the snapshot and the volume must be there
too) and `CI_RUNNER_COUNT` (runners on the droplet, default 2).

## Cost

$0.50 for each hour the droplet exists, billed by the second, plus $25 a
month for the volume and about $0.10 for the snapshot. It is up while CI
runs that build the engine and for 15 minutes after the last one: on a
quiet day nothing, on a day like 12 November (about 100 runs in 10 hours)
about 10 hours, $5. The run summary says, for each run, where its heavy
jobs went and why, which is the measure to watch.
