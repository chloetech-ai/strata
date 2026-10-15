"""Converts LoCoMo alone with the repository's own converter functions
(convert_memory_benchmarks.py needs the LongMemEval files for a full run).
Usage: convert_locomo_only.py ENGRAM_DIR DATA_ROOT"""
import importlib.util, os, sys
engram_dir, root = sys.argv[1], sys.argv[2]
spec = importlib.util.spec_from_file_location("cmb", os.path.join(engram_dir, "scripts", "convert_memory_benchmarks.py"))
cmb = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cmb)
rep = {}
_, units = cmb.convert_locomo(os.path.join(root, "raw", "locomo", "locomo10.json"), rep)
out = os.path.join(root, "locomo.jsonl")
size = cmb.write_jsonl(units, out)
assert cmb.validate(out) == len(units)
print(f"wrote {out} ({size} bytes, {len(units)} units)")
