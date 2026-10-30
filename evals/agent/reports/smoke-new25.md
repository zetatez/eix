# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: agentic_cli_args, agentic_edit_distance, agentic_glob, agentic_json_escape, agentic_ledger, agentic_ring_buffer, agentic_rle, agentic_roman, agentic_search_bounds, agentic_semver, agentic_token_bucket, agentic_toposort, agentic_utf8_truncate, math_angle, math_big_factorial, math_collatz, math_complex, math_crt, math_determinant, math_divisors, math_extended_gcd, math_isqrt, math_polynomial, math_prime_factors, math_triangle
- repeats: 1

## Summary

- score: **92.1** / 100
- pass: **24/25** (96%)
- axes: correctness 96, adherence 99, loops 90, tokens 89, time 90, cache 82
- mean duration: **27.2s**
- mean model steps: **5.16**
- mean tool calls: **4.28**
- prompt tokens: **860709**
- completion tokens: **59891**
- usage coverage: **100%** of model steps
- cache hit rate: **82.9%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| agentic_ring_buffer | agentic-coding | yes | 97 A | 15.3s | 4 | 3 | 25298 | 445 | 63% |
| agentic_json_escape | agentic-coding | yes | 95 A | 18.6s | 4 | 3 | 25834 | 1315 | 64% |
| agentic_search_bounds | agentic-coding | yes | 95 A | 14.9s | 5 | 4 | 32133 | 830 | 86% |
| agentic_roman | agentic-coding | no | 40 F | 16.4s | 4 | 3 | 26984 | 1027 | 84% |
| agentic_semver | agentic-coding | yes | 87 B | 28.7s | 8 | 7 | 56006 | 1827 | 86% |
| agentic_toposort | agentic-coding | yes | 99 A | 14.2s | 4 | 3 | 25811 | 707 | 84% |
| agentic_rle | agentic-coding | yes | 87 B | 58.0s | 7 | 8 | 48631 | 5850 | 84% |
| agentic_cli_args | agentic-coding | yes | 99 A | 12.0s | 4 | 3 | 25796 | 844 | 85% |
| agentic_token_bucket | agentic-coding | yes | 83 B | 43.0s | 9 | 8 | 67229 | 3227 | 81% |
| agentic_glob | agentic-coding | yes | 99 A | 11.3s | 4 | 3 | 25346 | 758 | 85% |
| agentic_utf8_truncate | agentic-coding | yes | 99 A | 16.8s | 4 | 3 | 25506 | 1031 | 85% |
| agentic_edit_distance | agentic-coding | yes | 94 A | 26.4s | 6 | 5 | 40549 | 2291 | 85% |
| agentic_ledger | agentic-coding | yes | 99 A | 18.0s | 4 | 3 | 26819 | 1478 | 84% |
| math_polynomial | mathematical-reasoning | yes | 99 A | 20.0s | 4 | 3 | 25492 | 1104 | 83% |
| math_complex | mathematical-reasoning | yes | 98 A | 21.5s | 4 | 3 | 25557 | 1682 | 85% |
| math_extended_gcd | mathematical-reasoning | yes | 96 A | 31.6s | 5 | 5 | 32766 | 1632 | 83% |
| math_prime_factors | mathematical-reasoning | yes | 99 A | 14.8s | 4 | 3 | 25828 | 670 | 82% |
| math_determinant | mathematical-reasoning | yes | 87 B | 52.1s | 7 | 6 | 50420 | 5400 | 84% |
| math_isqrt | mathematical-reasoning | yes | 91 A | 43.8s | 6 | 5 | 39790 | 5341 | 86% |
| math_triangle | mathematical-reasoning | yes | 99 A | 18.5s | 4 | 3 | 25810 | 1220 | 85% |
| math_angle | mathematical-reasoning | yes | 91 A | 24.8s | 6 | 5 | 39704 | 1857 | 85% |
| math_crt | mathematical-reasoning | yes | 91 A | 92.4s | 4 | 3 | 27787 | 13795 | 82% |
| math_big_factorial | mathematical-reasoning | yes | 85 B | 36.9s | 9 | 8 | 60702 | 3385 | 83% |
| math_divisors | mathematical-reasoning | yes | 99 A | 15.0s | 4 | 3 | 23971 | 1260 | 84% |
| math_collatz | mathematical-reasoning | yes | 98 A | 14.9s | 5 | 4 | 30940 | 915 | 85% |

## Failures

### agentic_roman (repeat 1)

- send exit: `0` timed_out=`False`
```
patch applied: src/lib.rs

```
```

running 1 test
.
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s


running 3 tests
. 1/3
parses_canonical_numerals --- FAILED
.
failures:

---- parses_canonical_numerals stdout ----

thread 'parses_canonical_numerals' (348073) panicked at tests/hidden_eval.rs:18:5:
assertion `left == right` failed
  left: None
 right: Some(1994)
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    parses_canonical_numerals

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass `--test hidden_eval`

```
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T142323Z-agentic_roman-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T142323Z-agentic_roman-r1/data/eix.sqlite3

```

