
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 4.00 terms, 875849 postings) |
| mean AND / OR result | 24 / 836333 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.013 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.61 | 0.0 | 0.03 | 0.88 | 0.77 | 0.030 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.61 | 0.0 | 0.01 | 0.44 | 0.30 | 0.024 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.88 | 0.0 | 0.14 | 1.14 | 0.89 | 0.031 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.86 | 0.0 | 0.18 | 1.11 | 1.01 | 0.019 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.49 | 0.0 | 0.03 | 0.91 | 0.85 | 0.029 (native) | — |
| Roaring | 11.72 | 9.25 | 0.0 | 0.08 | 1.33 | 0.91 | 0.012 (native) | — |
| Trie | 9.61 | 7.09 | 0.0 | 0.39 | 10.65 | 7.81 | 0.054 (native) | — |
| Run-Trie | 9.13 | 6.58 | 0.0 | 0.43 | 11.44 | 8.53 | 0.057 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.25 | 0.0 | 0.16 | 5.64 | 5.26 | 0.036 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: PEF (4.86 bits, 0.019 ms) → Roaring (9.25 bits, 0.012 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.449 |
| Uncompressed | AND | SvS + merge (branchless) | 0.665 |
| Uncompressed | AND | SvS + galloping | 0.014 |
| Uncompressed | AND | SvS + binary search | 0.081 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.021 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.315 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.262 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.013 |
| Uncompressed | AND | Small Adaptive | 0.024 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.052 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.014 |
| Uncompressed | AND | round-robin (LFTJ) | 0.050 |
| Uncompressed | AND | decode + SvS | 0.070 |
| BP128 | AND | leapfrog (NextGEQ) | 0.030 |
| BP128 | AND | round-robin (LFTJ) | 0.091 |
| BP128 | AND | decode + SvS | 0.673 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.024 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.059 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.263 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.031 |
| OptPFor | AND | round-robin (LFTJ) | 0.093 |
| OptPFor | AND | decode + SvS | 0.758 |
| PEF | AND | leapfrog (NextGEQ) | 0.019 |
| PEF | AND | round-robin (LFTJ) | 0.045 |
| PEF | AND | decode + SvS | 0.858 |
| Hybrid (bitmap + BP128) | AND | native | 0.029 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.029 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.067 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.888 |
| Roaring | AND | native | 0.012 |
| Roaring | AND | native, count only | 0.012 |
| Roaring | AND | leapfrog (NextGEQ) | 0.013 |
| Roaring | AND | round-robin (LFTJ) | 0.030 |
| Roaring | AND | decode + SvS | 0.419 |
| Trie | AND | native | 0.054 |
| Trie | AND | leapfrog (NextGEQ) | 0.098 |
| Trie | AND | round-robin (LFTJ) | 0.210 |
| Trie | AND | decode + SvS | 4.490 |
| Run-Trie | AND | native | 0.057 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.108 |
| Run-Trie | AND | round-robin (LFTJ) | 0.224 |
| Run-Trie | AND | decode + SvS | 4.825 |
| Roaring (roaring crate) | AND | native | 0.052 |
| Roaring (roaring crate) | AND | native, count only | 0.052 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.036 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.063 |
| Roaring (roaring crate) | AND | decode + SvS | 3.716 |
