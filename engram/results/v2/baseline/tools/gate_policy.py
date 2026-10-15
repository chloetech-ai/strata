"""The gated injection policy of a checkout's own default gate (the weights in
src/recall.rs `impl Default for Gate`), applied to engram-bench retrieval rows.
Same accounting as scripts/fit_gate.py's policy(): full injection at G >= hi,
brief (900 chars) at lo <= G < hi; coverage over answerable questions,
precision over all injections. Unlike fit_gate.py it does not refit.
Usage: gate_policy.py ENGRAM_DIR ROWS.jsonl [ROWS.jsonl ...] [--out gate_policy.json]"""
import json, re, sys


def gate_weights(engram_dir):
    src = open(f"{engram_dir}/src/recall.rs").read()
    m = re.search(r"impl Default for Gate \{.*?Gate \{([^}]*)\}", src, re.S)
    w = {k: float(v) for k, v in re.findall(r"(\w+):\s*(-?[0-9.]+)", m.group(1))} if m else {}
    if not all(k in w for k in ("a", "b", "c", "d", "e", "f", "hi", "lo")):
        raise ValueError(f"could not read the default Gate weights from {engram_dir}/src/recall.rs")
    return w


def policy(w, path):
    tot = ans = cov = inj = full = good = 0; chars = 0.0; always_cov = 0
    for l in open(path):
        r = json.loads(l); e = r["evidence"]
        g = (w["a"] * e["max_cos"] + w["b"] * e["max_cov"] + w["c"] * float(e["cue"]) + w["d"] * float(e["agree"])
             + w["e"] * e.get("cos_gap", 0.0) + w["f"] * e.get("bm_gap", 0.0))
        answerable = not (r["foreign"] or r["abstention"] or r["n_evidence"] == 0)
        mm = r["configs"].get("hybrid_0.5_priors", {})
        tot += 1; ans += answerable; always_cov += answerable and mm.get("pack4k", 0) > 0
        if g >= w["hi"]:
            inj += 1; full += 1; chars += mm.get("pack4k_chars", 3000) if answerable else 3000
            hit = answerable and mm.get("pack4k", 0) > 0
        elif g >= w["lo"]:
            inj += 1; chars += 900; hit = answerable and mm.get("pack_brief", 0) > 0
        else:
            hit = False
        cov += hit; good += hit
    return {"rows": tot, "answerable": ans, "inject_rate": inj / max(tot, 1), "full_rate": full / max(tot, 1),
            "precision": good / max(inj, 1), "coverage": cov / max(ans, 1), "chars_per_prompt": chars / max(tot, 1),
            "always_coverage": always_cov / max(ans, 1), "always_precision": always_cov / max(tot, 1)}


def main():
    argv = sys.argv[1:]
    out = None
    if "--out" in argv:
        i = argv.index("--out"); out = argv[i + 1]; del argv[i:i + 2]
    engram_dir, rows = argv[0], argv[1:]
    w = gate_weights(engram_dir)
    res = {"gate": w}
    for path in rows:
        p = res[path] = policy(w, path)
        print(f"{path}: inject rate {p['inject_rate']:.3f} (full {p['full_rate']:.3f}), precision {p['precision']:.3f} "
              f"(always {p['always_precision']:.3f}), evidence reaches the model {p['coverage']:.3f} (always {p['always_coverage']:.3f}), "
              f"{p['chars_per_prompt']:.0f} chars per prompt")
    if out:
        json.dump(res, open(out, "w"), indent=1)


if __name__ == "__main__":
    main()
