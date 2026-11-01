# Claude's relevance grades against TREC-COVID's

`umbrela.py` measures how the grades of `POST /v1/judge` (UMBRELA's
prompt, one row per request, 0 to 3) agree with TREC-COVID's human
judgments (0 to 2), row by row and in which of two rankings they prefer.
`umbrela_trec-covid.json` is its output.

- **Pool**: the first 25 queries by id; for each, the top 10 of BM25
  (k1 0.9, b 0.4, in the script) and the top 10 of dense retrieval (Cohere
  embed-english-v3, 1,024 dimensions), as the relevance lab pools rankings A
  and B: 464 rows, 429 of them judged by TREC.
- **Grader**: `claude-haiku-5-5`, the server's default judge model, with
  the prompt and passage `judge.rs` builds (title and text, each cut at
  4,000 characters). The container had no API key, so the prompts went
  through `claude -p` (no tools, no settings, no session, a one-line
  system prompt) rather than the Messages API the server calls. There
  the model thought before most answers (259 thinking tokens a row on
  average); the server asks without thinking, so its grades may differ
  somewhat from these.
- **Machine**: the 4-core cloud container (Intel Xeon @ 2.10 GHz), 31
  October 2026; grading 464 rows six at a time took 6.6 minutes.

## Results

| | |
|---|---|
| rows graded | 457 of 464 (7 answers reasoned without a grade; the server now asks such an answer once more for its grade) |
| relevant by both (TREC ≥ 1, Claude ≥ 2) | agreement 84%, Cohen's kappa 0.57 |
| highly relevant (TREC 2, Claude ≥ 2) | kappa 0.50 |
| TREC 0 rows Claude grades 2 or 3 | 27% (25 of 94): Claude is the more lenient |
| TREC 2 rows Claude grades 2 or 3 | 92% (242 of 263) |
| mean nDCG@10, BM25 / dense | TREC's judgments 0.529 / 0.785; Claude's 0.550 / 0.854 |
| queries where both name the same winner | 21 of the 24 that TREC's judgments decide (by more than 0.01) |
| Kendall's tau of the per-query differences | 0.32 |

Confusion (rows TREC judged):

| | Claude 0 | Claude 1 | Claude 2 | Claude 3 |
|---|---|---|---|---|
| TREC 0 | 7 | 62 | 21 | 4 |
| TREC 1 | 0 | 22 | 39 | 11 |
| TREC 2 | 1 | 20 | 134 | 108 |

So on these queries Claude's grades pick the better of two rankings as
TREC's do on seven queries in eight, and reproduce the size of the gap
less well (tau 0.32): they are good for telling which ranking is better
on queries nobody has judged, and a weaker stand-in for absolute nDCG.
The relevance lab shows both, and the agreement of its own grades with
Claude's (Cohen's linear-weighted kappa) once a set has both.

```sh
python3 umbrela.py --data ~/data/beir/trec-covid --cache grades.jsonl --out umbrela_trec-covid.json
```

The data directory is the collection as `strata/scripts/prepare_beir.py`
prepares it, with its vectors. `--cache` keeps each answer, so a run
stopped part way goes on where it stopped.
