
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 500 (mean 2.80 terms, 576257 postings) |
| mean AND / OR result | 309 / 562995 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.05 | 0.31 | 0.24 | 0.060 (SvS + galloping) | 1.756 (bitset window (4096)) |
| VByte | 10.33 | 9.61 | 0.0 | 0.05 | 1.80 | 1.71 | 0.127 (leapfrog (NextGEQ)) | 2.814 (bitset window (4096)) |
| BP128 | 10.07 | 7.69 | 0.0 | 0.05 | 0.83 | 0.61 | 0.076 (leapfrog (NextGEQ)) | 2.587 (bitset window (4096)) |
| BP128 (bitpacking crate) | 10.07 | 7.69 | 0.0 | 0.03 | 0.71 | 0.41 | 0.069 (leapfrog (NextGEQ)) | 2.422 (bitset window (4096)) |
| OptPFor | 7.13 | 5.92 | 0.0 | 0.21 | 1.25 | 0.81 | 0.080 (leapfrog (NextGEQ)) | 2.792 (bitset window (4096)) |
| Elias-Fano | 10.85 | 8.13 | 0.0 | 0.05 | 1.64 | 1.67 | 0.049 (leapfrog (NextGEQ)) | 2.494 (bitset window (4096)) |
| PEF | 7.04 | 4.94 | 0.0 | 0.14 | 1.53 | 1.33 | 0.040 (leapfrog (NextGEQ)) | 2.995 (bitset window (4096)) |
| Loom | 6.86 | 4.90 | 0.0 | 0.49 | 0.51 | 0.32 | 0.021 (native) | 0.509 (native) |
| Interpolative | 5.97 | 4.90 | 0.0 | 0.15 | 4.65 | 4.59 | 0.191 (leapfrog (NextGEQ)) | 5.033 (bitset window (4096)) |
| Entropic | 4.97 | 4.22 | 14.3 | 2.73 | 1.80 | 1.70 | 0.089 (leapfrog (NextGEQ)) | 1.306 (native) |
| Hybrid (bitmap + BP128) | 10.01 | 7.58 | 0.0 | 0.07 | 1.04 | 0.87 | 0.063 (native) | 1.859 (native) |
| Roaring | 11.72 | 9.32 | 0.0 | 0.12 | 2.48 | 1.66 | 0.029 (native) | 0.538 (native) |
| Pyramid | 10.35 | 6.20 | 0.0 | 0.30 | 1.09 | 0.55 | 0.023 (native) | 0.308 (native) |
| Trie | 9.61 | 7.18 | 0.0 | 0.62 | 7.55 | 5.20 | 0.176 (native) | 2.534 (native) |
| Run-Trie | 9.13 | 6.67 | 0.0 | 0.81 | 9.20 | 6.02 | 0.141 (native) | 2.688 (native) |
| Roaring (roaring crate) | 11.72 | 9.32 | 0.0 | 0.24 | 7.87 | 6.78 | 0.086 (leapfrog (NextGEQ)) | 4.126 (native) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.22 bits, 0.089 ms) → Loom (4.90 bits, 0.021 ms)
* OR: Entropic (4.22 bits, 1.306 ms) → Loom (4.90 bits, 0.509 ms) → Pyramid (6.20 bits, 0.308 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.361 |
| Uncompressed | AND | SvS + merge (branchless) | 1.417 |
| Uncompressed | AND | SvS + galloping | 0.060 |
| Uncompressed | AND | SvS + binary search | 0.345 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.112 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.597 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.598 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.473 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.080 |
| Uncompressed | AND | Small Adaptive | 0.076 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.115 |
| Uncompressed | OR | slices: heap | 6.693 |
| Uncompressed | OR | slices: fold (pairwise merge) | 2.273 |
| Uncompressed | OR | slices: bitmap | 1.778 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.067 |
| Uncompressed | AND | round-robin (LFTJ) | 0.102 |
| Uncompressed | AND | decode + SvS | 0.227 |
| Uncompressed | OR | heap | 3.336 |
| Uncompressed | OR | min-scan | 2.844 |
| Uncompressed | OR | bitset window (4096) | 1.756 |
| VByte | AND | leapfrog (NextGEQ) | 0.127 |
| VByte | AND | round-robin (LFTJ) | 0.193 |
| VByte | AND | decode + SvS | 0.840 |
| VByte | OR | heap | 4.363 |
| VByte | OR | min-scan | 3.289 |
| VByte | OR | bitset window (4096) | 2.814 |
| BP128 | AND | leapfrog (NextGEQ) | 0.076 |
| BP128 | AND | round-robin (LFTJ) | 0.106 |
| BP128 | AND | decode + SvS | 0.418 |
| BP128 | OR | heap | 3.701 |
| BP128 | OR | min-scan | 2.978 |
| BP128 | OR | bitset window (4096) | 2.587 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.069 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.104 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.325 |
| BP128 (bitpacking crate) | OR | heap | 3.831 |
| BP128 (bitpacking crate) | OR | min-scan | 3.001 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 2.422 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.080 |
| OptPFor | AND | round-robin (LFTJ) | 0.117 |
| OptPFor | AND | decode + SvS | 0.505 |
| OptPFor | OR | heap | 4.044 |
| OptPFor | OR | min-scan | 3.347 |
| OptPFor | OR | bitset window (4096) | 2.792 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.049 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.073 |
| Elias-Fano | AND | decode + SvS | 0.948 |
| Elias-Fano | OR | heap | 4.181 |
| Elias-Fano | OR | min-scan | 2.728 |
| Elias-Fano | OR | bitset window (4096) | 2.494 |
| PEF | AND | leapfrog (NextGEQ) | 0.040 |
| PEF | AND | round-robin (LFTJ) | 0.066 |
| PEF | AND | decode + SvS | 0.761 |
| PEF | OR | heap | 4.575 |
| PEF | OR | min-scan | 3.517 |
| PEF | OR | bitset window (4096) | 2.995 |
| Loom | AND | native | 0.021 |
| Loom | OR | native | 0.509 |
| Loom | AND | native, count only | 0.021 |
| Loom | OR | native, count only | 0.190 |
| Loom | AND | leapfrog (NextGEQ) | 0.042 |
| Loom | AND | round-robin (LFTJ) | 0.062 |
| Loom | AND | decode + SvS | 0.232 |
| Loom | OR | heap | 4.340 |
| Loom | OR | min-scan | 3.384 |
| Loom | OR | bitset window (4096) | 2.649 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.191 |
| Interpolative | AND | round-robin (LFTJ) | 0.313 |
| Interpolative | AND | decode + SvS | 2.736 |
| Interpolative | OR | heap | 6.457 |
| Interpolative | OR | min-scan | 5.439 |
| Interpolative | OR | bitset window (4096) | 5.033 |
| Entropic | AND | native | 0.092 |
| Entropic | OR | native | 1.306 |
| Entropic | AND | leapfrog (NextGEQ) | 0.089 |
| Entropic | AND | round-robin (LFTJ) | 0.144 |
| Entropic | AND | decode + SvS | 0.831 |
| Entropic | OR | heap | 5.487 |
| Entropic | OR | min-scan | 4.216 |
| Entropic | OR | bitset window (4096) | 4.171 |
| Hybrid (bitmap + BP128) | AND | native | 0.063 |
| Hybrid (bitmap + BP128) | OR | native | 1.859 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.071 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.094 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.770 |
| Hybrid (bitmap + BP128) | OR | heap | 5.521 |
| Hybrid (bitmap + BP128) | OR | min-scan | 4.662 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 4.751 |
| Roaring | AND | native | 0.029 |
| Roaring | OR | native | 0.538 |
| Roaring | AND | native, count only | 0.031 |
| Roaring | OR | native, count only | 0.407 |
| Roaring | AND | leapfrog (NextGEQ) | 0.044 |
| Roaring | AND | round-robin (LFTJ) | 0.062 |
| Roaring | AND | decode + SvS | 0.409 |
| Roaring | OR | heap | 4.714 |
| Roaring | OR | min-scan | 3.617 |
| Roaring | OR | bitset window (4096) | 2.946 |
| Pyramid | AND | native | 0.023 |
| Pyramid | OR | native | 0.308 |
| Pyramid | AND | native, count only | 0.020 |
| Pyramid | OR | native, count only | 0.089 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.043 |
| Pyramid | AND | round-robin (LFTJ) | 0.065 |
| Pyramid | AND | decode + SvS | 0.225 |
| Pyramid | OR | heap | 5.513 |
| Pyramid | OR | min-scan | 4.934 |
| Pyramid | OR | bitset window (4096) | 5.274 |
| Trie | AND | native | 0.176 |
| Trie | OR | native | 2.534 |
| Trie | AND | leapfrog (NextGEQ) | 0.184 |
| Trie | AND | round-robin (LFTJ) | 0.261 |
| Trie | AND | decode + SvS | 1.366 |
| Trie | OR | heap | 6.993 |
| Trie | OR | min-scan | 5.848 |
| Trie | OR | bitset window (4096) | 5.423 |
| Run-Trie | AND | native | 0.141 |
| Run-Trie | OR | native | 2.688 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.209 |
| Run-Trie | AND | round-robin (LFTJ) | 0.288 |
| Run-Trie | AND | decode + SvS | 1.717 |
| Run-Trie | OR | heap | 7.313 |
| Run-Trie | OR | min-scan | 5.790 |
| Run-Trie | OR | bitset window (4096) | 5.799 |
| Roaring (roaring crate) | AND | native | 0.089 |
| Roaring (roaring crate) | OR | native | 4.126 |
| Roaring (roaring crate) | AND | native, count only | 0.084 |
| Roaring (roaring crate) | OR | native, count only | 0.704 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.086 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.104 |
| Roaring (roaring crate) | AND | decode + SvS | 3.486 |
| Roaring (roaring crate) | OR | heap | 5.969 |
| Roaring (roaring crate) | OR | min-scan | 5.668 |
| Roaring (roaring crate) | OR | bitset window (4096) | 5.061 |
