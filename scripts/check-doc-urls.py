#!/usr/bin/env python3
"""Do the downloads, images and packages the docs tell people to install exist?

The guide's `# install` blocks are shown, not run (docs/guide/run.py runs
this checkout's builds instead), so a pinned release that was never
published, an image tag that was never pushed or a Homebrew tap that does
not exist passes every other check. This reads the docs a newcomer follows
(README.md and the packages' READMEs, docs/**/*.md but the rendered site
and the release snapshots, the website's own pages and docs/install.sh) and
checks, over the network, each of:

- a release download URL (github.com/<repo>/releases/download/<tag>/<asset>,
  with the shell variables a block sets, `v=0.3.3 p=macos-arm64`, put in)
  and a release's `expanded_assets` page: the release has that asset;
- the installer, docs/install.sh: the latest release (docs/site/versions.json)
  has the server's archive and its .sha256 for every platform, and those of
  every program a page installs with `--with`;
- an image, ghcr.io/<owner>/<name>[:tag] (no tag is :latest): the tag exists;
- `brew install <owner>/<tap>/<formula>`: the tap's repository has the formula;
- `pip install chloedb[extras]`: PyPI's newest chloedb is the latest release
  the docs name, and has those extras;
- the latest release named by docs/site/versions.json: its server, Python and
  JavaScript releases exist, PyPI has it, and so does the image.

    python3 scripts/check-doc-urls.py           # check, exit 1 if anything is missing
    python3 scripts/check-doc-urls.py --list    # what would be checked, and from where

GH_TOKEN or GITHUB_TOKEN, when set, is sent to api.github.com only (for its
rate limit). The workflow Docs URLs (.github/workflows/docs-urls.yml) runs
this when docs change on main, after each release workflow, every night,
and by hand.
"""
import argparse
import html
import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
REPO = "chloetech-ai/chloedb"
PLATFORMS = ["linux-x86_64", "linux-aarch64", "macos-arm64", "macos-x86_64"]
INSTALL_WITH = {"crawl": "chloedb-crawl", "sheets": "chloedb-sheets", "ctx": "chloectx",
                "chloedb-crawl": "chloedb-crawl", "chloedb-sheets": "chloedb-sheets", "chloectx": "chloectx"}
UA = "chloedb-doc-url-check"
TIMEOUT = 30


def doc_files():
    files = [ROOT / "README.md", ROOT / "docs" / "install.sh"]
    files += sorted(ROOT.glob("*/README.md"))
    files += sorted(p for p in (ROOT / "docs").rglob("*.md")
                    if not p.relative_to(ROOT / "docs").as_posix().startswith("site/docs/"))
    files += sorted((ROOT / "docs" / "site").glob("*.html"))
    return [p for p in files if p.is_file()]


FENCE = re.compile(r"^[ \t]*(```|~~~)[^\n]*\n(.*?)^[ \t]*\1[ \t]*$", re.S | re.M)
PRE = re.compile(r"<pre[^>]*>(.*?)</pre>", re.S)
TAGS = re.compile(r"<[^>]+>")
ASSIGN = re.compile(r"(?:^|[\s;])([A-Za-z_][A-Za-z0-9_]*)=([^\s;&|'\"`]+)")


def blocks(path):
    """The text of a file in pieces: (piece, line, is a code block)."""
    text = path.read_text(errors="replace")
    if path.suffix == ".html":
        for m in PRE.finditer(text):
            yield html.unescape(TAGS.sub("", m.group(1))), text.count("\n", 0, m.start()) + 1, True
        yield html.unescape(text), 1, False
    elif path.suffix == ".md":
        for m in FENCE.finditer(text):
            yield m.group(2), text.count("\n", 0, m.start()) + 2, True
        # The prose, with each block blanked to its newlines so lines keep their numbers.
        yield FENCE.sub(lambda m: "\n" * m.group(0).count("\n"), text), 1, False
    else:
        yield text, 1, True


def expand(block):
    """A block's text with the shell variables it sets put in."""
    env = {}
    for m in ASSIGN.finditer(block):
        env.setdefault(m.group(1), m.group(2))
    return re.sub(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?", lambda m: env.get(m.group(1), m.group(0)), block)


RELEASE_URL = re.compile(r"https://github\.com/([\w.-]+/[\w.-]+)/releases/download/([^/\s]+)/([^\s)\"'`<>\]]+)")
EXPANDED = re.compile(r"https://github\.com/([\w.-]+/[\w.-]+)/releases/expanded_assets/([^\s)\"'`<>\]]+)")
IMAGE = re.compile(r"\bghcr\.io/([\w.-]+/[\w.-]+)(?::([\w][\w.-]*))?")
BREW = re.compile(r"\bbrew install (?:--\S+ )*([\w-]+)/([\w-]+)/([\w@.+-]+)")
PIP = re.compile(r"\bpip3? install (?:-U |--upgrade |-q )*[\"']?chloedb(?:\[([\w,-]+)\])?(?:==([\w.]+))?[\"']?(?=[\s;|&]|$)", re.M)
INSTALLER = re.compile(r"install\.sh\b[^\n]*?--with[ =]([\w,-]+)")


def unresolved(s):
    return "$" in s or "<" in s or "{" in s or "&lt;" in s


def token(text, m):
    """The whole word a match starts, up to a space or quote: a match that
    stops at a placeholder (`<version>`, `$v`) is not a name to check."""
    end = re.match(r"[^\s\"'`)\]]*", text[m.start():]).end()
    return text[m.start():m.start() + end]


def find(files):
    """Every check, as {key: [where]}; key is a tuple (kind, ...)."""
    found = {}

    def add(key, where):
        found.setdefault(key, [])
        if where not in found[key]:
            found[key].append(where)

    for path in files:
        rel = path.relative_to(ROOT).as_posix()
        for piece, line, code in blocks(path):
            text = expand(piece) if code else piece
            def at(m):
                return f"{rel}:{line + text.count(chr(10), 0, m.start())}"
            for m in RELEASE_URL.finditer(text):
                repo, tag, asset = m.groups()
                if not unresolved(token(text, m)):
                    add(("asset", repo, tag, asset.rstrip(".,:;")), at(m))
            for m in EXPANDED.finditer(text):
                repo, tag = m.groups()
                if not unresolved(token(text, m)):
                    add(("release", repo, tag.rstrip(".,:;")), at(m))
            if not code:
                continue
            # Commands count only in code blocks; prose may name them to
            # say they do not work.
            for m in IMAGE.finditer(text):
                name, tag = m.group(1), m.group(2) or "latest"
                if name.startswith("chloetech-ai/") and not unresolved(token(text, m)) \
                        and not text[m.end():m.end() + 1] == ":":
                    add(("image", name, tag), at(m))
            for m in BREW.finditer(text):
                add(("brew",) + m.groups(), at(m))
            for m in PIP.finditer(text):
                extras = tuple(sorted(x for x in (m.group(1) or "").split(",") if x))
                add(("pypi", extras, m.group(2) or ""), at(m))
            for m in INSTALLER.finditer(text):
                for w in m.group(1).split(","):
                    add(("with", w), at(m))
            if "install.sh" in text and re.search(r"\|\s*(?:\w+=\S+\s+)*sh\b", text):
                add(("with", "server"), f"{rel}:{line}")
    return found


class Net:
    def __init__(self):
        self.token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        self.cache = {}

    def get(self, url, headers=None, method="GET"):
        """(status, body bytes or None)."""
        key = (url, method)
        if key in self.cache:
            return self.cache[key]
        h = {"User-Agent": UA}
        if self.token and urllib.parse.urlsplit(url).hostname == "api.github.com":
            h["Authorization"] = f"Bearer {self.token}"
        h.update(headers or {})
        req = urllib.request.Request(url, headers=h, method=method)
        try:
            with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
                out = (r.status, r.read() if method == "GET" else None)
        except urllib.error.HTTPError as e:
            out = (e.code, None)
        except (urllib.error.URLError, OSError) as e:
            out = (0, str(e).encode())
        self.cache[key] = out
        return out

    def json(self, url, headers=None):
        status, body = self.get(url, headers)
        if status == 200 and body:
            return json.loads(body)
        return None if status == 404 else {"__error__": status, "__body__": (body or b"").decode(errors="replace")[:200]}

    def release(self, repo, tag):
        """The release's asset names, None if there is no such release."""
        r = self.json(f"https://api.github.com/repos/{repo}/releases/tags/{urllib.parse.quote(tag)}")
        if r is None:
            return None
        if "__error__" in r:
            raise RuntimeError(f"api.github.com: HTTP {r['__error__']} for release {tag} {r['__body__']}")
        return {a["name"] for a in r.get("assets", [])}

    def image(self, name, tag):
        tok = self.json(f"https://ghcr.io/token?scope=repository:{name}:pull")
        if not tok or "__error__" in tok:
            return False
        accept = ",".join([
            "application/vnd.oci.image.index.v1+json",
            "application/vnd.oci.image.manifest.v1+json",
            "application/vnd.docker.distribution.manifest.list.v2+json",
            "application/vnd.docker.distribution.manifest.v2+json",
        ])
        status, _ = self.get(f"https://ghcr.io/v2/{name}/manifests/{tag}",
                             {"Authorization": f"Bearer {tok['token']}", "Accept": accept}, method="HEAD")
        return status == 200

    def pypi(self, name, version=""):
        url = f"https://pypi.org/pypi/{name}/{version + '/' if version else ''}json"
        return self.json(url, {"Accept": "application/json"})


def latest_release():
    return json.loads((ROOT / "docs" / "site" / "versions.json").read_text())["latest"]


def check(found, net):
    """Yields (ok, what, where)."""
    latest = latest_release()
    server_tag = f"chloedb-server-v{latest}"
    vj = ["docs/site/versions.json"]

    for tag in (server_tag, f"chloedb-py-v{latest}", f"chloedb-js-v{latest}"):
        yield net.release(REPO, tag) is not None, f"release {tag} (the latest release, says versions.json)", vj

    # The installer, and the programs pages install with it, on every platform.
    wanted = {w for (kind, *rest) in found if kind == "with" for w in rest}
    if wanted:
        progs = ["chloedb-server"]
        for w in sorted(wanted - {"server"}):
            for p in (["chloedb-crawl", "chloedb-sheets", "chloectx"] if w == "all" else [INSTALL_WITH.get(w, w)]):
                if p not in progs:
                    progs.append(p)
        assets = net.release(REPO, server_tag) or set()
        for prog in progs:
            where = sorted({x for (kind, *rest), ws in found.items() if kind == "with" for x in ws
                            if prog == "chloedb-server" or INSTALL_WITH.get(rest[0]) == prog or rest[0] == "all"})
            missing = [f"{prog}-{p}.tar.gz{s}" for p in PLATFORMS for s in ("", ".sha256")
                       if f"{prog}-{p}.tar.gz{s}" not in assets]
            yield not missing, (f"install.sh installs {prog} {latest} on every platform"
                                + (f": {server_tag} has no {', '.join(missing)}" if missing else "")), where

    for key, where in sorted(found.items()):
        kind = key[0]
        if kind == "asset":
            _, repo, tag, asset = key
            assets = net.release(repo, tag)
            ok = assets is not None and asset in assets
            why = "" if ok else (" (no such release)" if assets is None else " (the release has no such asset)")
            yield ok, f"https://github.com/{repo}/releases/download/{tag}/{asset}{why}", where
        elif kind == "release":
            _, repo, tag = key
            yield net.release(repo, tag) is not None, f"release {tag} of {repo}", where
        elif kind == "image":
            _, name, tag = key
            yield net.image(name, tag), f"image ghcr.io/{name}:{tag}", where
        elif kind == "brew":
            _, owner, tap, formula = key
            ok = any(net.get(f"https://raw.githubusercontent.com/{owner}/homebrew-{tap}/{ref}/Formula/{formula}.rb",
                             method="HEAD")[0] == 200 for ref in ("HEAD", "main", "master"))
            yield ok, f"brew install {owner}/{tap}/{formula} (Formula/{formula}.rb in github.com/{owner}/homebrew-{tap})", where
        elif kind == "pypi":
            _, extras, pinned = key
            info = net.pypi("chloedb", pinned)
            if not info or "__error__" in info:
                yield False, f"pip install chloedb{'==' + pinned if pinned else ''}: PyPI has no such release", where
                continue
            have = info["info"]["version"]
            if not pinned and not extras:
                yield have == latest, f"pip install chloedb gives {have}, the latest release the docs name is {latest}", where
            missing = sorted(set(extras) - set(info["info"].get("provides_extra") or []))
            if extras:
                yield not missing, (f"chloedb {have} on PyPI has the extras [{','.join(extras)}]"
                                    + (f": no {', '.join(missing)}" if missing else "")), where

    # The image of the latest release (a version tag is pushed by image.yml).
    if any(k[0] == "image" and k[1] == "chloetech-ai/chloedb-server" for k in found):
        yield net.image("chloetech-ai/chloedb-server", latest), f"image ghcr.io/chloetech-ai/chloedb-server:{latest} (the latest release)", vj


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--list", action="store_true", help="list what would be checked, without the network")
    args = ap.parse_args()
    found = find(doc_files())
    if args.list:
        print(f"latest release (docs/site/versions.json): {latest_release()}")
        for key, where in sorted(found.items()):
            print(" ".join(str(k) for k in key), "  <-", ", ".join(where))
        return 0
    net = Net()
    bad = 0
    try:
        for ok, what, where in check(found, net):
            print(("ok    " if ok else "FAIL  ") + what + ("" if ok else "\n      named in " + ", ".join(where)))
            bad += not ok
    except RuntimeError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    print(f"{bad} missing" if bad else "everything the docs install exists")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
