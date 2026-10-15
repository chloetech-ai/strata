import json, os, subprocess, sys, concurrent.futures as cf
ex = json.load(open('exchange.json')); pr = json.load(open('prompts.json'))
known = "[n:1] requirement: Cancellation fees are planned for the next sprint (not built yet)."  # irrelevant to the check; keeps the shape
def fill(t):
    return t.replace("{request}", ex['request']).replace("{commands}", "(none)").replace("{answer}", ex['answer']).replace("{known}", "(nothing yet)")
env = {"PATH": os.environ["PATH"], "HOME": os.path.abspath("home"), "ANTHROPIC_BASE_URL": os.environ.get("ANTHROPIC_BASE_URL", ""), "ENGRAM_DISABLE": "1"}
def call(args):
    which, i = args
    p = subprocess.run(["claude", "-p", "--system-prompt", pr['system'], "--tools", "", "--no-session-persistence", "--output-format", "json",
                        "--setting-sources", "project", "--strict-mcp-config", "--model", "haiku"], input=fill(pr[which]), capture_output=True, text=True, env=env, timeout=180, cwd="home")
    out = json.loads(p.stdout)
    return which, i, out.get("result", ""), out.get("total_cost_usd")
jobs = [(w, i) for i in range(6) for w in ("old", "new")]
res = list(cf.ThreadPoolExecutor(6).map(call, jobs))
json.dump(res, open('results.json', 'w'), indent=1)
for w in ("old", "new"):
    rs = [r for r in res if r[0] == w]
    bad = 0
    for _, i, text, c in rs:
        reqs = [json.loads(l) for l in text.splitlines() if l.strip().startswith("{")]
        open_q = [x for x in reqs if x.get("kind") == "requirement" and any(k in x["text"].lower() for k in ("undecided", "must be settled", "need deciding", "needs deciding", "open question", "to be decided", "still open", "not yet settled", "not yet answered", "before the fee is built", "before building"))]
        bad += bool(open_q)
        print(w, i, f"${c:.4f}", [x['kind'] for x in reqs], [x['text'][:140] for x in open_q])
    print(w, "runs with an open-question requirement:", bad, "/", len(rs))
