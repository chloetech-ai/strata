import json
import os
import signal
import subprocess
import sys
import time
import urllib.request
import uuid

import pytest

import strata
from strata import ingest
from conftest import free_port, server_binary

USERS = 50
TABLES = [
    "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD, item KEYWORD)",
    "CREATE TABLE users (id KEYWORD PRIMARY KEY, n INT DEFAULT 0)",
]


def click(i):
    return {"id": i, "usr": f"u{i % USERS}", "item": f"p{i % 97}"}


def to_ops(e):
    c = json.loads(e.value)
    return [
        {"op": "insert", "table": "events", "doc": c},
        {"op": "add", "table": "users", "key": c["usr"], "field": "n", "value": 1},
    ]


class Server:
    """A strata-server on one port, killed with SIGKILL and started again
    on the same database and port."""

    def __init__(self, path):
        self.binary = server_binary()
        if not self.binary:
            pytest.skip("strata-server is not built")
        self.path, self.port, self.proc = path, free_port(), None
        self.url = f"http://127.0.0.1:{self.port}"

    def start(self):
        args = [self.binary, "--db", str(self.path), "--listen", f"127.0.0.1:{self.port}", "--grpc", "off", "--log", "warn"]
        if not os.path.exists(self.path):
            schema = self.path.with_suffix(".schema.json")
            schema.write_text(json.dumps({"table": "seed", "key": "id", "fields": [{"name": "id", "type": "keyword"}]}))
            args += ["--schema", str(schema)]
        self.proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        for _ in range(300):
            try:
                urllib.request.urlopen(self.url + "/v1/health", timeout=1)
                return self
            except OSError:
                if self.proc.poll() is not None:
                    raise RuntimeError(self.proc.stderr.read().decode())
                time.sleep(0.05)
        raise RuntimeError("strata-server did not start")

    def kill(self):
        self.proc.send_signal(signal.SIGKILL)
        self.proc.wait(timeout=10)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            self.proc.wait(timeout=10)


@pytest.fixture
def server(tmp_path):
    s = Server(tmp_path / "events.strata").start()
    c = strata.Client(s.url)
    for ddl in TABLES:
        c.sql(ddl)
    c.insert([{"id": f"u{u}"} for u in range(USERS)], table="users")
    yield s
    s.stop()


class Memory(ingest.Log):
    """Partitions in memory, read in order as a consumer given all of them
    reads them: a log for tests without Kafka."""

    def __init__(self, parts, per_poll=40, hook=None):
        self.parts, self.per_poll, self.hook = parts, per_poll, hook
        self.at, self.moved, self.committed, self.polled = {}, False, {}, 0

    def open(self, start):
        self.at = {p: o or 0 for p, o in start(list(self.parts)).items()}
        self.moved = True

    def poll(self, timeout):
        out = []
        for (t, p), values in self.parts.items():
            i = self.at[(t, p)]
            out += [ingest.Event(t, p, o, 1_761_800_000_000 + o, None, values[o]) for o in range(i, min(i + self.per_poll, len(values)))]
            self.at[(t, p)] = min(i + self.per_poll, len(values))
        self.polled += len(out)
        if self.hook:
            self.hook(self)
        if not out:
            time.sleep(min(timeout, 0.02))
        return out

    def assigned(self):
        return list(self.parts)

    def changed(self):
        c, self.moved = self.moved, False
        return c

    def seek(self, offsets):
        self.at.update({p: o or 0 for p, o in offsets.items()})

    def commit(self, offsets):
        self.committed.update(offsets)


def clicks(partitions, per):
    # Event i goes to partition i % partitions, as a key's hash would.
    parts = {("clicks", p): [] for p in range(partitions)}
    for i in range(partitions * per):
        parts[("clicks", i % partitions)].append(json.dumps(click(i)).encode())
    return parts


def totals(c):
    return c.query("SELECT COUNT(*) FROM events").scalar(), c.query("SELECT SUM(n) FROM users").scalar()


def test_a_consumer_resumes_where_the_server_stopped(server):
    c = strata.Client(server.url)
    parts = clicks(3, 1000)
    # A consumer that stops partway, then a new one reading from offset 0:
    # it starts after the server's offsets, so nothing is read twice.
    first = ingest.consume(c, Memory(parts), to_ops, max_events=1200, seconds_per_stream=0.2)
    assert (first["events"], first["written"], first["skipped"]) == (1200, 2400, 0)
    assert totals(c) == (1200, 1200)
    log = Memory(parts)
    second = ingest.consume(c, log, to_ops, idle=0.3, seconds_per_stream=0.2)
    assert (second["events"], second["skipped"]) == (1800, 0), second
    assert totals(c) == (3000, 3000)
    assert {s["name"]: s["offset"] for s in c.sources()} == {"clicks/0": 999, "clicks/1": 999, "clicks/2": 999}
    assert log.committed == {("clicks", p): 1000 for p in range(3)}
    # Nothing new: nothing written.
    assert ingest.consume(c, Memory(parts), to_ops, idle=0.2)["events"] == 0
    # A log that starts over, as a Kafka group whose offsets were lost does,
    # is skipped by the server's offsets.
    replay = Memory(parts)
    replay.open = lambda start: (start(list(parts)), setattr(replay, "at", {p: 0 for p in parts}))
    again = ingest.consume(c, replay, to_ops, idle=0.2)
    assert (again["written"], again["skipped"]) == (0, 6000), again
    assert totals(c) == (3000, 3000)


def test_a_server_killed_mid_stream_loses_and_duplicates_nothing(server):
    c = strata.Client(server.url)
    kills = []

    def kill_once(log):
        # Once a third of the events are read, SIGKILL the server and start
        # it again; the consumer is mid-stream.
        if not kills and log.polled >= 3000:
            kills.append(log.polled)
            server.kill()
            server.start()

    stats = ingest.consume(c, Memory(clicks(3, 3000), hook=kill_once), to_ops, idle=0.5, seconds_per_stream=0.5)
    assert kills and stats["retries"] >= 1, stats
    assert totals(c) == (9000, 9000)
    assert [s["offset"] for s in c.sources()] == [2999, 2999, 2999]


def test_a_bad_event_stops_the_consumer_after_the_events_before_it(server):
    c = strata.Client(server.url)
    parts = {("clicks", 0): [json.dumps(click(i)).encode() for i in range(100)]}
    parts[("clicks", 0)][60] = b"not json"
    with pytest.raises(ValueError):
        ingest.consume(c, Memory(parts), to_ops, idle=0.2)
    assert totals(c) == (60, 60)
    assert c.source("clicks/0") == 59
    # A write the server refuses (no such table) is not retried either.
    with pytest.raises(strata.StrataError, match="HTTP 400"):
        ingest.consume(c, Memory({("views", 0): [b"{}"]}), lambda e: {"op": "insert", "table": "views", "doc": {}}, idle=0.2)
    # None writes only the offset.
    assert ingest.consume(c, Memory({("views", 0): [b"{}"] * 5}), lambda e: None, idle=0.2)["written"] == 0
    assert c.source("views/0") == 4


KAFKA = os.environ.get("STRATA_KAFKA")
CONSUMER = """
import json, sys, time, strata
from strata import ingest
url, topic, group, idle = sys.argv[1:5]
# At most 5,000 events a second, so that a consumer is still writing when it
# or the server is killed: unpaced, a fast machine wrote all the second
# consumer's events in two streams of a quarter second, before the server's
# kill, and the consumer never retried.
def to_ops(e):
    time.sleep(0.0002)
    c = json.loads(e.value)
    return [{"op": "insert", "table": "events", "doc": c}, {"op": "add", "table": "users", "key": c["usr"], "field": "n", "value": 1}]
# A killed consumer's partitions move once its session times out.
config = {"session.timeout.ms": 6000, "heartbeat.interval.ms": 500}
print(json.dumps(ingest.kafka(strata.Client(url), topic, to_ops, brokers=sys.argv[5], group=group, config=config, idle=float(idle) if idle != "none" else None, seconds_per_stream=0.25, retry_for=60)))
"""


@pytest.mark.skipif(not KAFKA, reason="STRATA_KAFKA (a Kafka or Redpanda broker, host:port) is not set")
def test_kafka_exactly_once_across_consumer_and_server_restarts(server):
    """Against a real broker: a consumer killed with SIGKILL partway, then
    a second one during which the server is killed with SIGKILL and started
    again. The events are written once each."""
    admin = pytest.importorskip("confluent_kafka.admin")
    from confluent_kafka import Producer

    topic, group, n = f"clicks-{uuid.uuid4().hex[:8]}", f"strata-{uuid.uuid4().hex[:8]}", 30_000
    a = admin.AdminClient({"bootstrap.servers": KAFKA})
    a.create_topics([admin.NewTopic(topic, num_partitions=3, replication_factor=1)])[topic].result(30)
    p = Producer({"bootstrap.servers": KAFKA, "linger.ms": 5})
    for i in range(n):
        p.produce(topic, key=f"u{i % USERS}", value=json.dumps(click(i)))
        if i % 5000 == 0:
            p.poll(0)
    p.flush(30)
    c = strata.Client(server.url)

    def written():
        return sum(s["offset"] + 1 for s in c.sources() if s["name"].startswith(topic))

    def until(cond, what, secs=120):
        t0 = time.monotonic()
        while not cond():
            assert time.monotonic() - t0 < secs, what
            time.sleep(0.05)

    def consumer(idle):
        return subprocess.Popen([sys.executable, "-c", CONSUMER, server.url, topic, group, idle, KAFKA], stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    first = consumer("none")
    until(lambda: written() >= 5000, "the first consumer wrote nothing")
    first.send_signal(signal.SIGKILL)
    first.wait(timeout=10)
    at_kill = written()
    assert at_kill < n, "the first consumer finished before its kill"

    second = consumer("3")
    until(lambda: written() >= at_kill + 5000, "the second consumer wrote nothing")
    server.kill()
    server.start()
    out, err = second.communicate(timeout=180)
    assert second.returncode == 0, err.decode()
    stats = json.loads(out)
    assert stats["retries"] >= 1, stats
    assert totals(c) == (n, n)
    # Each partition's offset is its last message's.
    from confluent_kafka import Consumer, TopicPartition

    k = Consumer({"bootstrap.servers": KAFKA, "group.id": group})
    ends = {f"{topic}/{q}": k.get_watermark_offsets(TopicPartition(topic, q), timeout=10)[1] - 1 for q in range(3)}
    k.close()
    assert {s["name"]: s["offset"] for s in c.sources() if s["name"].startswith(topic)} == ends
