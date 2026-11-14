# Sharing a studio over the network

The studio has no login: on a laptop it listens on 127.0.0.1 and whoever can
reach it can build apps on your model account. To let someone else in, run
it behind two guards:

- **A shared key.** `deploy/gate.mjs` is the only thing that listens
  outside. It refuses every request without the key, which comes once in the
  link (`http://host:port/?key=...`) and is then kept in a cookie for 30
  days. Map tiles and an app's files load from inside sandboxed frames that
  send no cookie, so those two paths pass without it (files are named by ids
  nobody can guess). The key travels in the clear over plain HTTP, so treat
  the link like a password and change the key to cut everyone off.
- **A spending limit.** `STRATA_APPS_SPEND_CAP_USD` caps what the model may
  cost in this studio, counted from what Claude Code reports per answer and
  kept in `DATA/spend.json` across restarts. Past it, every build and change
  fails with "this studio has reached its spending limit". Apps' own
  questions (`app.ask`) count apart: all the apps together stop at
  `STRATA_APPS_ASK_CAP_USD` dollars a day (a quarter of the cap by default,
  $20 when there is no cap), and builds get the rest of the cap, so a
  published app's visitors cannot stop the builds, nor one app's visitors
  the other apps for more than a day; one app's questions stop at
  `STRATA_APPS_ASK_USD_PER_DAY` dollars a day (default 2). Calls already
  running finish, so the total can pass the cap by a build or two.
  `GET /api/status` shows the count (`spend`, with `asksUsd`, `asksToday`
  and `asksCap`).

Also set `STRATA_APPS_EVOLVE=off`, so the studio builds nothing by itself
from what people do.

## On Kubernetes

`deploy/kube.yaml` is one pod (the engine, its chloedb-server, the gate) and a
NodePort Service, with the data on one node's disk. It needs no image of its
own: it copies chloedb-server out of the cluster's image, and fetches
chloedb-apps at one pushed commit of this repository.

```bash
deploy/kube.sh --ref <full commit sha> --node <node name> \
  --image ghcr.io/chloetech-ai/chloedb-server@sha256:<digest> --cap 25
```

It expects the Secret `anthropic` (key `key`, an Anthropic API key) in the
namespace `strata`, makes the gate's key once (Secret `apps-studio-gate`),
and prints the link. The node's firewall must let the NodePort (30730) in;
on DigitalOcean Kubernetes, a firewall of its own tagged with the cluster's
tag (`k8s:<cluster id>`) does it, and costs nothing.

- New version: run `kube.sh` again with the new `--ref`; the data stays.
- New key (cuts off everyone who has the link): `kubectl -n strata delete
  secret apps-studio-gate`, run `kube.sh` again.
- Raise the limit: run `kube.sh` again with a higher `--cap`.
- Take it down: `kubectl -n strata delete deploy/apps-studio svc/apps-studio
  secret/apps-studio-gate`, delete the firewall, and delete
  `/var/lib/strata-apps-studio` on the node if the data should go too.
