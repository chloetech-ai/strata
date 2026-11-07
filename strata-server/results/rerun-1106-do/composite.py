"""composite.py OUT BASE ENGINE=DIR ...: a reports directory for rank.py with each named engine's passes, recall, load, writes and streams from DIR, and the other engines' from BASE."""
import json, shutil, sys
from pathlib import Path
out, base = Path(sys.argv[1]), Path(sys.argv[2])
take = dict(a.split("=", 1) for a in sys.argv[3:])
if "es" in take: take.setdefault("es-trial", take["es"])
out.mkdir(parents=True, exist_ok=True)
s = json.loads((base / "summary.json").read_text())
for e in s["engines"] + [e for e in take if e not in s["engines"]]:
    src = Path(take.get(e, base))
    for f in src.glob(f"run-{e}*.json*"):
        rest = f.name[len(f"run-{e}"):]
        if rest.split(".")[0] in ("", "-pass2", "-pass3"):
            shutil.copy(f, out / f.name)
for e, d in take.items():
    ts = json.loads((Path(d) / "summary.json").read_text())
    if e not in s["engines"]: s["engines"].append(e)
    for key in ("quality", "unavailable", "load", "writes", "stream", "differs"):
        s.setdefault(key, {})
        if (ts.get(key) or {}).get(e) is not None: s[key][e] = ts[key][e]
        else: s[key].pop(e, None)
(out / "summary.json").write_text(json.dumps(s))
