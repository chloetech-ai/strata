import os
import shutil
import socket
import subprocess
import time
import urllib.request

import pytest

import strata

BOOKS = {
    "table": "books",
    "key": "id",
    "fields": [
        {"name": "id", "type": "keyword"},
        {"name": "title", "type": "text"},
        {"name": "description", "type": "text"},
        {"name": "genres", "type": "keyword", "multi": True},
        {"name": "year", "type": "int"},
        {"name": "rating", "type": "float"},
        {"name": "emb", "type": "vector", "embed": {"from": ["title", "description"], "model": "hash:32"}},
    ],
}

WORDS = "river mountain dragon detective ship garden war letter city winter star secret".split()


def book(i):
    w = WORDS
    return {
        "id": f"b{i:04d}",
        "title": f"{w[i % 12]} {w[(i * 7) % 12]}",
        "description": " ".join(w[(i + j) % 12] for j in range(8)),
        "genres": ["fantasy" if i % 2 else "mystery", "classic" if i % 5 == 0 else "new"],
        "year": 1900 + i % 120,
        "rating": round(1 + (i % 40) / 10, 1),
    }


@pytest.fixture
def db(tmp_path):
    with strata.Database.create(tmp_path / "books.strata", BOOKS) as d:
        d.insert([book(i) for i in range(200)])
        yield d


def server_binary():
    path = os.environ.get("STRATA_SERVER") or os.path.join(os.path.dirname(__file__), "..", "..", "strata-server", "target", "release", "strata-server")
    return path if os.path.exists(path) else shutil.which("strata-server")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def serve(tmp_path):
    """Starts strata-server on a database; returns its URL. Skips when the
    binary is not built (cargo build --release in strata-server, or
    STRATA_SERVER=path)."""
    binary = server_binary()
    if not binary:
        pytest.skip("strata-server is not built")
    procs = []

    def start(db_path, *args):
        port = free_port()
        p = subprocess.Popen([binary, "--db", str(db_path), "--listen", f"127.0.0.1:{port}", "--grpc", "off", *args], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        procs.append(p)
        url = f"http://127.0.0.1:{port}"
        for _ in range(100):
            try:
                urllib.request.urlopen(url + "/v1/health", timeout=1)
                return url
            except urllib.error.HTTPError:
                return url  # up, behind a token
            except OSError:
                if p.poll() is not None:
                    raise RuntimeError(p.stderr.read().decode())
                time.sleep(0.1)
        raise RuntimeError("strata-server did not start")

    yield start
    for p in procs:
        p.terminate()
        p.wait(timeout=10)


def tone_wav():
    """A WAV file of a tenth of a second of a 440 Hz tone, 16 kHz."""
    import io, math, struct, wave

    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(b"".join(struct.pack("<h", int(8000 * math.sin(2 * math.pi * 440 * i / 16000))) for i in range(1600)))
    return buf.getvalue()
