#!/usr/bin/env python3
"""CI's on-demand runner on DigitalOcean: one droplet, made when CI needs
it and destroyed when CI has been idle, so it bills only while CI runs.

  ci_runner.py want     ci.yml's job `runner`: prints `label=<runs-on>`
                        for $GITHUB_OUTPUT: the droplet's label if one of
                        its runners is free (with CI_RUNNER=overflow, only
                        when GitHub's free runners are full)
                        (making the droplet, and waiting for it to come up,
                        when there is none). It never fails: whatever goes wrong,
                        the jobs run on GitHub's runners as before.
  ci_runner.py place    ci-runner.yml's job `place`, which runs main's code
                        with the secrets: `want` for a run that is not
                        main's (a pull request's, the merge queue's, a
                        tag's), posted as commit status `ci-runner` on the
                        run's commit, for that run's job `runner`, which
                        gets no secret.
  ci_runner.py placed   ci.yml's job `runner` outside main: waits for
                        `place`'s status and prints `label=<runs-on>`, or
                        GitHub's label if none comes.
  ci_runner.py reap     ci-runner.yml: destroys the droplet once no CI run
                        has been queued or running for --idle seconds, or
                        once it is older than --max-age, or when it came up
                        without a runner.
  ci_runner.py image    ci-runner.yml: builds the snapshot droplets start
                        from (provision.sh on a fresh Ubuntu droplet, then
                        a snapshot `ci-runner-base-<time>`), keeping the
                        newest two.

The state between droplets (cargo's registry, the build folders, the
runners' work folders) lives on the volume `ci-runner-cache`, attached to
each new droplet: a powered-off droplet bills as much as a running one, a
volume a tenth of a cent an hour per 10 GB.

The two secrets are in the environment `ci-runner`, which only main may
use: a branch's copy of ci.yml runs with neither (audit 4, X-I2).

Environment: DIGITALOCEAN_TOKEN (the DigitalOcean API), GH_ADMIN_TOKEN (a
token with this repository's Administration permission, read and write,
which registering runners takes), GITHUB_TOKEN (reads the workflow runs);
CI_RUNNER_SIZE (c-16), CI_RUNNER_REGION (nyc1), CI_RUNNER_COUNT (2, the
runners on the droplet), CI_RUNNER_VOLUME_GB (250), CI_RUNNER ("off" turns
it all off, "overflow" uses it only when GitHub's runners are full). No token is ever printed.
"""
import argparse
import base64
import datetime as dt
import json
import os
import pathlib
import sys
import time
import urllib.error
import urllib.request

REPO = os.environ.get("GITHUB_REPOSITORY", "chloetech-ai/chloedb")
LABEL = "chloedb-c16"
TAG = "ci-runner"
NAME = "ci-runner-c16"
VOLUME = "ci-runner-cache"
SNAPSHOT_PREFIX = "ci-runner-base-"
HERE = pathlib.Path(__file__).resolve().parent
FALLBACK = "ubuntu-latest"


def env(name, default=""):
    return os.environ.get(name) or default


def log(*a):
    print(*a, file=sys.stderr, flush=True)


def call(url, token, method="GET", body=None, ok=(200, 201, 202, 204)):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else {})
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            j = json.loads(raw)
        except ValueError:
            j = {"message": raw[:200].decode(errors="replace")}
        if e.code in ok:
            return e.code, j
        raise RuntimeError(f"{method} {url.split('?')[0]}: HTTP {e.code}: {j.get('message', '')}") from None


def do(path, method="GET", body=None):
    return call("https://api.digitalocean.com/v2" + path, env("DIGITALOCEAN_TOKEN"), method, body)[1]


def gh(path, method="GET", body=None, admin=False):
    token = env("GH_ADMIN_TOKEN") if admin else env("GITHUB_TOKEN", env("GH_ADMIN_TOKEN"))
    return call("https://api.github.com" + path, token, method, body)[1]


def age_s(stamp):
    return (dt.datetime.now(dt.timezone.utc) - dt.datetime.fromisoformat(stamp.replace("Z", "+00:00"))).total_seconds()


def droplets():
    return do(f"/droplets?tag_name={TAG}&per_page=50").get("droplets", [])


def runners():
    rs = gh(f"/repos/{REPO}/actions/runners?per_page=100", admin=True).get("runners", [])
    return [r for r in rs if any(l["name"] == LABEL for l in r.get("labels", []))]


def snapshot():
    snaps = [s for s in do("/snapshots?resource_type=droplet&per_page=200").get("snapshots", []) if s["name"].startswith(SNAPSHOT_PREFIX)]
    return max(snaps, key=lambda s: s["created_at"]) if snaps else None


def volume(region):
    vs = [v for v in do(f"/volumes?name={VOLUME}&region={region}").get("volumes", [])]
    if vs:
        return vs[0]
    log(f"creating volume {VOLUME} in {region}")
    return do("/volumes", "POST", {
        "name": VOLUME, "region": region, "size_gigabytes": int(env("CI_RUNNER_VOLUME_GB", "250")),
        "filesystem_type": "ext4", "description": "CI runner build folders (.github/ci-runner)", "tags": [TAG],
    })["volume"]


def create(region, size, count):
    snap = snapshot()
    if not snap:
        raise RuntimeError("no ci-runner-base snapshot: run the CI runner workflow's image job")
    if region not in snap["regions"]:
        raise RuntimeError(f"snapshot {snap['name']} is not in {region}")
    vol = volume(region)
    if vol.get("droplet_ids"):
        raise RuntimeError(f"volume {VOLUME} is attached to droplet {vol['droplet_ids']}")
    token = gh(f"/repos/{REPO}/actions/runners/registration-token", "POST", admin=True)["token"]
    # Each runner gets CPUs of its own (systemd's AllowedCPUs, picked up by
    # boot.sh's daemon-reload), so one job's build does not slow the other's
    # timing tests: replay's M/D/1 test measured 15.5 ms of service for 4
    # while the other runner compiled (#466). nproc in a job is its share.
    cpus = (
        "n=$(nproc); k=$((n / %d)); for i in $(seq 1 %d); do d=/etc/systemd/system/ci-runner-$i.service.d; "
        "mkdir -p $d; printf '[Service]\\nAllowedCPUs=%%s-%%s\\n' $(((i - 1) * k)) $((i * k - 1)) > $d/cpus.conf; done"
    ) % (count, count)
    user_data = "#cloud-config\nruncmd:\n  - [bash, -c, %s]\n  - [/opt/ci-runner/boot.sh, %s, %s, '%d']\n" % (json.dumps(cpus), json.dumps(token), LABEL, count)
    d = do("/droplets", "POST", {
        "name": NAME, "region": region, "size": size, "image": int(snap["id"]),
        "volumes": [vol["id"]], "tags": [TAG], "user_data": user_data, "monitoring": False, "ipv6": False,
    })["droplet"]
    log(f"created droplet {d['id']} ({size} in {region}) from {snap['name']}")
    return d


def hosted_load():
    """(Linux jobs on GitHub's runners now, those queued for one over a
    minute), over every workflow of this repository."""
    running = queued = 0
    for status in ("in_progress", "queued"):
        for run in gh(f"/repos/{REPO}/actions/runs?status={status}&per_page=100").get("workflow_runs", []):
            for job in gh(f"/repos/{REPO}/actions/runs/{run['id']}/jobs?per_page=100").get("jobs", []):
                labels = set(job.get("labels") or [])
                if LABEL in labels or any(l.startswith("macos") for l in labels):
                    continue
                if job["status"] == "in_progress" and job.get("runner_group_name") == "GitHub Actions":
                    running += 1
                elif job["status"] == "queued" and age_s(job["created_at"]) > 60:
                    queued += 1
    return running, queued


def want(args):
    if env("CI_RUNNER") == "off":
        return FALLBACK, "turned off (variable CI_RUNNER)"
    if not env("DIGITALOCEAN_TOKEN") or not env("GH_ADMIN_TOKEN"):
        return FALLBACK, "no DIGITALOCEAN_TOKEN or CI_RUNNER_ADMIN_TOKEN secret (a fork's pull request has neither)"
    # By default the droplet takes the longest jobs of every run that
    # builds the engine (the server's binary, ChloeDB's and the server's
    # tests), which it runs in about half the time of GitHub's 4-core
    # runners; it is destroyed after 15 idle minutes (`reap`), so it bills
    # only while CI is busy. CI_RUNNER=overflow uses it only when GitHub's
    # runners are full (jobs waiting for one), or nearly (fewer than
    # --headroom of the --limit free, since this run is about to start a
    # dozen jobs of its own), as GitHub's cost nothing.
    if env("CI_RUNNER") == "overflow" and not args.force:
        limit = int(env("CI_RUNNER_HOSTED_LIMIT", str(args.limit)))
        running, queued = hosted_load()
        if not queued and running + args.headroom < limit:
            return FALLBACK, f"GitHub's runners have room ({running} of {limit} busy, none waiting)"
        log(f"GitHub's runners are full: {running} of {limit} busy, {queued} jobs waiting")
    deadline = time.time() + args.wait
    made = False
    while True:
        rs = runners()
        online = [r for r in rs if r["status"] == "online"]
        if any(not r["busy"] for r in online):
            return LABEL, f"{sum(not r['busy'] for r in online)} of {len(online)} runners free"
        if online:
            # Every runner is busy with another run's jobs: GitHub's runners
            # are as quick as waiting for one.
            return FALLBACK, f"all {len(online)} runners busy"
        ds = droplets()
        if ds and ds[0]["status"] == "off":
            return FALLBACK, f"droplet {ds[0]['id']} powered itself off: its boot failed (the reaper destroys it)"
        if not ds and not made:
            create(env("CI_RUNNER_REGION", "nyc1"), env("CI_RUNNER_SIZE", "c-16"), int(env("CI_RUNNER_COUNT", "2")))
            made = True
        elif ds and age_s(ds[0]["created_at"]) > args.boot and not made:
            return FALLBACK, f"droplet {ds[0]['id']} is {int(age_s(ds[0]['created_at']))} s old and has no runner online"
        if time.time() > deadline:
            return FALLBACK, f"no runner online after {args.wait} s"
        time.sleep(10)


STATUS = "ci-runner"


def run_url():
    """The CI run and attempt `place` places, as a status's target_url."""
    return f"https://github.com/{REPO}/actions/runs/{env('RUN_ID')}/attempts/{env('RUN_ATTEMPT', '1')}"


def post(state, description):
    gh(f"/repos/{REPO}/statuses/{env('RUN_SHA')}", "POST", {"state": state, "context": STATUS, "description": description[:140], "target_url": run_url()})


def place(args):
    """Waits for the CI run's job `runner` to start (it starts when the run
    builds the engine; skipped, the run needs no droplet), then posts where
    its heavy jobs run."""
    post("pending", "placing the heavy jobs")
    deadline = time.time() + args.patience
    while True:
        jobs = gh(f"/repos/{REPO}/actions/runs/{env('RUN_ID')}/attempts/{env('RUN_ATTEMPT', '1')}/jobs?per_page=100").get("jobs", [])
        job = next((j for j in jobs if j["name"] == "runner"), None)
        if job and job["status"] == "completed":
            post("success", f"label={FALLBACK}: the run has no heavy jobs")
            return
        if job and job["status"] == "in_progress":
            break
        if time.time() > deadline:
            post("success", f"label={FALLBACK}: the run's job runner did not start")
            return
        time.sleep(10)
    try:
        label, why = want(args)
    except Exception as e:  # never fail CI over its runner
        label, why = FALLBACK, f"error: {e}"
    log(f"runs-on: {label} ({why})")
    post("success", f"label={label}: {why}")


def placed(args):
    """`place`'s label for this run and attempt, once it posted it, or
    GitHub's: when no status came within --start seconds (main's ci-runner.yml
    did not run for it), or no final one within --wait."""
    want_url = run_url()
    started = time.time()
    while True:
        try:
            ss = gh(f"/repos/{REPO}/commits/{env('RUN_SHA')}/statuses?per_page=100")
        except Exception as e:  # never fail CI over its runner
            return FALLBACK, f"error: {e}"
        mine = [x for x in ss if x.get("context") == STATUS and x.get("target_url") == want_url]
        done = next((x for x in mine if x["state"] == "success"), None)
        if done:
            label, _, why = done["description"].partition(": ")
            label = label.removeprefix("label=")
            return (label if label in (LABEL, FALLBACK) else FALLBACK), why or "placed"
        waited = time.time() - started
        if not mine and waited > args.start:
            return FALLBACK, f"no {STATUS} status in {args.start} s (ci-runner.yml's job place did not run)"
        if waited > args.wait:
            return FALLBACK, f"no runner placed in {args.wait} s"
        time.sleep(10)


def ci_activity():
    """(busy, seconds since the last CI run ended)."""
    busy = False
    for status in ("queued", "in_progress", "waiting", "requested", "pending"):
        if gh(f"/repos/{REPO}/actions/workflows/ci.yml/runs?status={status}&per_page=1").get("total_count", 0):
            busy = True
    last = gh(f"/repos/{REPO}/actions/workflows/ci.yml/runs?status=completed&per_page=5").get("workflow_runs", [])
    idle = min((age_s(r["updated_at"]) for r in last), default=1e9)
    return busy, idle


def reap(args):
    if not env("DIGITALOCEAN_TOKEN"):
        log("no DIGITALOCEAN_TOKEN: nothing to reap")
        return
    stop = time.time() + args.patience
    while True:
        ds = droplets()
        if not ds:
            log("no CI droplet")
            return
        rs = runners() if env("GH_ADMIN_TOKEN") else []
        working = any(r["busy"] for r in rs)
        online = any(r["status"] == "online" for r in rs)
        busy, idle = ci_activity()
        kept = False
        for d in ds:
            age = age_s(d["created_at"])
            why = None
            if age > args.max_age and (not working or age > args.max_age + 2 * 3600):
                why = f"older than {args.max_age} s"
            elif d["status"] == "off":
                why = "it powered itself off (its boot failed, or someone stopped it)"
            elif not online and age > args.boot + 300:
                why = "no runner came online"
            elif not busy and not working and idle > args.idle:
                why = f"CI idle for {int(idle)} s"
            if why:
                log(f"destroying droplet {d['id']} ({int(age)} s old): {why}")
                do(f"/droplets/{d['id']}", "DELETE")
            else:
                kept = True
                log(f"keeping droplet {d['id']} ({int(age)} s old): CI {'busy' if busy or working else f'idle for {int(idle)} s'}")
        left = stop - time.time()
        if not kept or left <= 0:
            return
        # Check again when the idle time would run out (or in two minutes
        # while CI is busy), as long as this run's patience lasts: the next
        # CI run that ends starts a new one, and the schedule backs it up.
        time.sleep(max(30, min(120 if busy or working else args.idle - idle + 15, left)))


def image(args):
    files = {n: base64.b64encode((HERE / n).read_bytes()).decode() for n in ("provision.sh", "boot.sh", "job-started.sh")}
    script = ["#!/bin/bash", "set -euxo pipefail", "mkdir -p /root/ci-runner && cd /root/ci-runner"]
    for n, b in files.items():
        script.append(f"echo {b} | base64 -d > {n} && chmod +x {n}")
    script += [f"./provision.sh {args.toolchain} > /var/log/ci-runner-provision.log 2>&1 || exit 1", "rm -rf /root/ci-runner", "poweroff"]
    region = env("CI_RUNNER_REGION", "nyc1")
    d = do("/droplets", "POST", {
        "name": "ci-runner-image", "region": region, "size": "s-4vcpu-8gb", "image": "ubuntu-24-04-x64",
        "tags": [TAG + "-image"], "user_data": "\n".join(script) + "\n", "monitoring": False,
    })["droplet"]
    log(f"building on droplet {d['id']}")
    try:
        deadline = time.time() + 40 * 60
        while do(f"/droplets/{d['id']}")["droplet"]["status"] != "off":
            if time.time() > deadline:
                raise RuntimeError("provisioning did not finish in 40 minutes")
            time.sleep(20)
        name = SNAPSHOT_PREFIX + dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d-%H%M")
        a = do(f"/droplets/{d['id']}/actions", "POST", {"type": "snapshot", "name": name})["action"]
        while (a := do(f"/actions/{a['id']}")["action"])["status"] == "in-progress":
            time.sleep(15)
        if a["status"] != "completed":
            raise RuntimeError(f"snapshot {a['status']}")
        log(f"snapshot {name}")
    finally:
        do(f"/droplets/{d['id']}", "DELETE")
    snaps = sorted((s for s in do("/snapshots?resource_type=droplet&per_page=200")["snapshots"] if s["name"].startswith(SNAPSHOT_PREFIX)), key=lambda s: s["created_at"])
    for s in snaps[:-2]:
        log(f"deleting old snapshot {s['name']}")
        do(f"/snapshots/{s['id']}", "DELETE")


def status(args):
    for d in droplets():
        print(f"droplet {d['id']} {d['name']} {d['size_slug']} {d['region']['slug']} {d['status']}, {int(age_s(d['created_at']))} s old")
    for r in runners():
        print(f"runner {r['name']}: {r['status']}{', busy' if r['busy'] else ''}")
    print(f"GitHub's runners: {'%d busy, %d waiting' % hosted_load()}")


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    w = sub.add_parser("want")
    w.add_argument("--wait", type=int, default=240, help="seconds to wait for a new droplet's runners")
    w.add_argument("--boot", type=int, default=360, help="a droplet older than this without a runner is broken")
    w.add_argument("--limit", type=int, default=60, help="jobs GitHub runs at once for the account (CI_RUNNER_HOSTED_LIMIT)")
    w.add_argument("--headroom", type=int, default=6, help="overflow when fewer slots than this are free")
    w.add_argument("--force", action="store_true", help="use the droplet even while GitHub has room (to test it)")
    r = sub.add_parser("reap")
    r.add_argument("--idle", type=int, default=900)
    r.add_argument("--max-age", type=int, default=12 * 3600)
    r.add_argument("--boot", type=int, default=360)
    r.add_argument("--patience", type=int, default=1200, help="seconds to keep checking")
    pl = sub.add_parser("place")
    for a, kw in [("--wait", dict(type=int, default=240)), ("--boot", dict(type=int, default=360)), ("--limit", dict(type=int, default=60)), ("--headroom", dict(type=int, default=6))]:
        pl.add_argument(a, **kw)
    pl.add_argument("--force", action="store_true")
    pl.add_argument("--patience", type=int, default=600, help="seconds to wait for the run's job runner to start")
    pd = sub.add_parser("placed")
    pd.add_argument("--start", type=int, default=120, help="seconds to wait for place's first status")
    pd.add_argument("--wait", type=int, default=420, help="seconds to wait for its last")
    sub.add_parser("status")
    i = sub.add_parser("image")
    i.add_argument("--toolchain", default="1.100.0")
    args = p.parse_args()
    if args.cmd in ("want", "placed"):
        try:
            label, why = want(args) if args.cmd == "want" else placed(args)
        except Exception as e:  # never fail CI over its runner
            label, why = FALLBACK, f"error: {e}"
        log(f"runs-on: {label} ({why})")
        print(f"label={label}")
        if env("GITHUB_STEP_SUMMARY"):
            with open(env("GITHUB_STEP_SUMMARY"), "a") as f:
                f.write(f"Heavy jobs run on **{label}**: {why}.\n")
    elif args.cmd == "place":
        place(args)
    elif args.cmd == "status":
        status(args)
    elif args.cmd == "reap":
        reap(args)
    else:
        image(args)


if __name__ == "__main__":
    main()
