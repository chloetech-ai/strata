"""Event logs into a strata-server, each event written once.

`kafka` reads Kafka (or Redpanda) topics into a server. Each partition is a
stream source, ``<prefix><topic>/<partition>``, whose last offset the server
keeps in the transaction of the rows its events made. The consumer starts
each partition it is given after the offset the server has, not after the
offsets its group committed to Kafka, so an event is written once across
consumer restarts, server restarts and rebalances::

    import json
    import strata
    from strata import ingest

    def click(e):
        c = json.loads(e.value)
        return [
            {"op": "insert", "table": "events", "doc": {"id": c["id"], "usr": c["usr"], "item": c["item"]}},
            {"op": "add", "table": "users", "key": c["usr"], "field": "n_events", "value": 1},
        ]

    ingest.kafka(strata.Client("http://127.0.0.1:7700"), "clicks", click, brokers="127.0.0.1:9092")

It needs ``confluent-kafka`` (``pip install confluent-kafka``, or the package's ``kafka`` extra). `consume`
runs the same loop over any log of numbered partitions (a `Log`).
"""

from __future__ import annotations

import http.client
import json
import time
from dataclasses import dataclass
from typing import Any, Callable, Iterable, Iterator, Mapping, Sequence

from ._strata import StrataError
from .client import Client, _default

Partition = tuple[str, int]


@dataclass
class Event:
    """One record of a log: Kafka's message."""

    topic: str
    partition: int
    offset: int
    # Milliseconds since the epoch, when the log has it.
    timestamp: int | None
    key: bytes | None
    value: bytes | None
    headers: Sequence[tuple[str, bytes]] = ()


class Log:
    """What `consume` reads: partitions of events numbered by offset, each
    read in order. A Kafka consumer is one (`KafkaLog`)."""

    def open(self, start: Callable[[list[Partition]], dict[Partition, int | None]]) -> None:
        """Starts reading. Whenever partitions are given to this reader,
        `start(partitions)` says the offset to read each from (None: from
        its beginning)."""
        raise NotImplementedError

    def poll(self, timeout: float) -> list[Event]:
        """The events that came within `timeout` seconds, perhaps none."""
        raise NotImplementedError

    def assigned(self) -> list[Partition]:
        raise NotImplementedError

    def changed(self) -> bool:
        """Whether partitions were given or taken since the last call."""
        raise NotImplementedError

    def seek(self, offsets: Mapping[Partition, int | None]) -> None:
        """Reads each partition from its offset next (None: its beginning)."""
        raise NotImplementedError

    def commit(self, offsets: Mapping[Partition, int]) -> None:
        """Records that the events before these offsets are written. The
        server's offsets are what resume a stream; this is for the log's
        own tools (Kafka's consumer lag)."""

    def close(self) -> None:
        pass


def ops_of(e: Event) -> list:
    """The default for `to_ops`: the event's value is JSON, an operation or
    one event's operations as {"ops": [...]}."""
    j = json.loads(e.value) if e.value else None
    if j is None:
        return []
    return list(j["ops"]) if isinstance(j, Mapping) and "ops" in j else [j]


def _retryable(err: BaseException) -> bool:
    # A server that is down, restarting or refusing for now (429, 5xx), or a
    # stream it cut short.
    if isinstance(err, StrataError):
        status = getattr(err, "status", None)
        return status is not None and (status == 429 or status >= 500)
    return isinstance(err, (OSError, http.client.HTTPException))


def consume(
    client: Client,
    log: Log,
    to_ops: Callable[[Event], Any] | None = None,
    *,
    prefix: str = "",
    seconds_per_stream: float = 1.0,
    max_events: int | None = None,
    idle: float | None = None,
    retry_for: float = 60.0,
) -> dict:
    """Writes `log`'s events to `client`'s server through `/v1/stream`, each
    once, until `max_events` events were read or none came for `idle`
    seconds while the consumer had partitions (both None: until
    interrupted).

    `to_ops(event)` gives the event's write operations (a list, or one
    operation); the server applies them together, in one transaction with
    the event's offset. None or [] writes nothing but the offset. A stream
    lasts `seconds_per_stream`, then the next starts, so the log learns
    what was written that often. When the server cannot be reached or
    refuses for now, the consumer waits and resumes every partition from
    the server's offsets, for up to `retry_for` seconds before it raises.

    Returns counts: {"events", "written", "skipped", "streams"} of the
    streams that completed, and "retries".
    """
    ops = to_ops or ops_of

    def name(p: Partition) -> str:
        return f"{prefix}{p[0]}/{p[1]}"

    stats = {"events": 0, "written": 0, "skipped": 0, "streams": 0, "retries": 0}

    def retrying(f: Callable[[], Any], since: float | None = None) -> Any:
        give_up, wait = (since or time.monotonic()) + retry_for, 0.25
        while True:
            try:
                return f()
            except Exception as e:
                if not _retryable(e) or time.monotonic() + wait > give_up:
                    raise
                stats["retries"] += 1
                time.sleep(wait)
                wait = min(wait * 2, 5.0)

    def offsets(parts: list[Partition]) -> dict[Partition, int | None]:
        have = {s["name"]: s["offset"] for s in client.sources()}
        return {p: (have[name(p)] + 1 if name(p) in have else None) for p in parts}

    def line(e: Event) -> bytes:
        src: dict = {"name": name((e.topic, e.partition)), "offset": e.offset}
        if e.timestamp is not None:
            src["ts"] = e.timestamp
        o = ops(e)
        if isinstance(o, Mapping):
            o = [o]
        j = {"source": src, "ops": list(o)} if o else {"op": "source", "source": src}
        return json.dumps(j, default=_default).encode()

    log.open(lambda parts: retrying(lambda: offsets(parts)))
    log.changed()
    # Events read but not yet sent: a stream ends when partitions move, and
    # those read with the move go first in the next.
    pending: list[Event] = []
    last_event = time.monotonic()
    read = 0
    # When the server stopped answering, if it has, and the next wait.
    failing: float | None = None
    wait = 0.25
    while max_events is None or read < max_events:
        if not pending:
            pending = log.poll(0.1)
            if log.changed() or not log.assigned():
                # Idle counts from when the consumer has its partitions.
                last_event = time.monotonic()
        if not pending:
            if idle is not None and time.monotonic() - last_event >= idle:
                break
            continue
        last_event = time.monotonic()
        sent: dict[Partition, int] = {}
        # An event `to_ops` failed on: the stream ends before it.
        bad: list[Exception] = []

        def chunks() -> Iterator[bytes]:
            nonlocal pending, read, last_event
            until = time.monotonic() + seconds_per_stream
            batch = pending
            while True:
                if max_events is not None:
                    batch, pending = batch[: max_events - read], batch[max_events - read :]
                else:
                    pending = []
                if batch:
                    last_event = time.monotonic()
                    lines = []
                    for e in batch:
                        try:
                            lines.append(line(e))
                        except Exception as err:
                            bad.append(err)
                            break
                        sent[(e.topic, e.partition)] = e.offset
                    read += len(lines)
                    if lines:
                        yield b"\n".join(lines) + b"\n"
                    if bad:
                        return
                if (max_events is not None and read >= max_events) or time.monotonic() >= until:
                    return
                batch = log.poll(min(0.1, max(until - time.monotonic(), 0.0)))
                if log.changed():
                    pending = batch
                    return

        try:
            done = client._stream(chunks())
        except Exception as e:
            if bad or not _retryable(e):
                raise
            # Some of the stream may be written: read on from where the
            # server's offsets say.
            failing = failing or time.monotonic()
            if time.monotonic() - failing > retry_for:
                raise
            pending = []
            stats["retries"] += 1
            time.sleep(wait)
            wait = min(wait * 2, 5.0)
            retrying(lambda: log.seek(offsets(log.assigned())), failing)
            continue
        failing, wait = None, 0.25
        stats["streams"] += 1
        stats["events"] += done["lines"]
        stats["written"] += done["written"]
        stats["skipped"] += done["skipped"]
        log.commit({p: o + 1 for p, o in sent.items()})
        if bad:
            raise bad[0]
    return stats


class KafkaLog(Log):
    """A Kafka consumer of `topics` in consumer group `group`, as a `Log`.
    `config` adds to or overrides the consumer's librdkafka settings; Kafka
    does not commit offsets itself (enable.auto.commit is off)."""

    def __init__(self, topics: str | Iterable[str], brokers: str, group: str = "strata", config: Mapping[str, Any] | None = None):
        from confluent_kafka import Consumer

        self.topics = [topics] if isinstance(topics, str) else list(topics)
        conf = {"bootstrap.servers": brokers, "group.id": group, "auto.offset.reset": "earliest", **(config or {}), "enable.auto.commit": False}
        self.cooperative = "cooperative" in str(conf.get("partition.assignment.strategy", ""))
        self.consumer = Consumer(conf)
        self._changed = False

    def open(self, start: Callable[[list[Partition]], dict[Partition, int | None]]) -> None:
        from confluent_kafka import OFFSET_BEGINNING

        def on_assign(consumer: Any, parts: list) -> None:
            at = start([(p.topic, p.partition) for p in parts])
            for p in parts:
                o = at[(p.topic, p.partition)]
                p.offset = OFFSET_BEGINNING if o is None else o
            if self.cooperative:
                consumer.incremental_assign(parts)
            else:
                consumer.assign(parts)
            self._changed = True

        def on_revoke(consumer: Any, parts: list) -> None:
            self._changed = True

        self.consumer.subscribe(self.topics, on_assign=on_assign, on_revoke=on_revoke, on_lost=on_revoke)

    def poll(self, timeout: float) -> list[Event]:
        from confluent_kafka import KafkaError, KafkaException

        out = []
        for m in self.consumer.consume(num_messages=1000, timeout=timeout):
            err = m.error()
            if err is not None:
                if err.code() == KafkaError._PARTITION_EOF:
                    continue
                raise KafkaException(err)
            kind, ts = m.timestamp()
            out.append(Event(m.topic(), m.partition(), m.offset(), ts if kind else None, m.key(), m.value(), m.headers() or ()))
        return out

    def assigned(self) -> list[Partition]:
        return [(p.topic, p.partition) for p in self.consumer.assignment()]

    def changed(self) -> bool:
        c, self._changed = self._changed, False
        return c

    def seek(self, offsets: Mapping[Partition, int | None]) -> None:
        from confluent_kafka import OFFSET_BEGINNING, TopicPartition

        for (t, p), o in offsets.items():
            self.consumer.seek(TopicPartition(t, p, OFFSET_BEGINNING if o is None else o))

    def commit(self, offsets: Mapping[Partition, int]) -> None:
        from confluent_kafka import KafkaException, TopicPartition

        if offsets:
            try:
                self.consumer.commit(offsets=[TopicPartition(t, p, o) for (t, p), o in offsets.items()], asynchronous=True)
            except KafkaException:
                pass  # a partition taken meanwhile; the server has the offsets

    def close(self) -> None:
        self.consumer.close()


def kafka(
    client: Client,
    topics: str | Iterable[str],
    to_ops: Callable[[Event], Any] | None = None,
    *,
    brokers: str,
    group: str = "strata",
    config: Mapping[str, Any] | None = None,
    **options: Any,
) -> dict:
    """Writes Kafka `topics` to `client`'s server, each event once
    (`consume`, whose options it takes: prefix, seconds_per_stream,
    max_events, idle, retry_for). Consumers of one `group` share the
    partitions, as Kafka's consumers do."""
    log = KafkaLog(topics, brokers, group, config)
    try:
        return consume(client, log, to_ops, **options)
    finally:
        log.close()
