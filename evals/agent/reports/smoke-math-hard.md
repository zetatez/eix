# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: math_binomial_mod, math_convex_hull, math_def_continuity, math_def_group, math_def_limit, math_matrix_pow, math_mod_sqrt, math_partition_mod, math_proof_cantor, math_proof_induction_odds, math_proof_inf_primes, math_proof_sqrt2, math_theorem_rolle, math_totient_sum
- repeats: 1

## Summary

- score: **94.2** / 100
- pass: **13/14** (93%)
- axes: correctness 93, adherence 100, loops 100, tokens 100, time 96, cache 86
- mean duration: **16.1s**
- mean model steps: **2.29**
- mean tool calls: **1.29**
- prompt tokens: **193204**
- completion tokens: **20682**
- usage coverage: **100%** of model steps
- cache hit rate: **84.3%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| math_proof_sqrt2 | mathematical-reasoning | yes | 99 A | 4.7s | 1 | 0 | 5896 | 489 | 87% |
| math_proof_inf_primes | mathematical-reasoning | no | 40 F | 6.1s | 1 | 0 | 5905 | 760 | 87% |
| math_proof_induction_odds | mathematical-reasoning | yes | 99 A | 3.2s | 1 | 0 | 5917 | 306 | 87% |
| math_def_limit | mathematical-reasoning | yes | 99 A | 4.6s | 1 | 0 | 5917 | 497 | 87% |
| math_def_continuity | mathematical-reasoning | yes | 99 A | 5.1s | 1 | 0 | 5900 | 427 | 87% |
| math_def_group | mathematical-reasoning | yes | 99 A | 6.7s | 1 | 0 | 5883 | 852 | 87% |
| math_theorem_rolle | mathematical-reasoning | yes | 99 A | 3.6s | 1 | 0 | 5912 | 440 | 87% |
| math_proof_cantor | mathematical-reasoning | yes | 98 A | 19.2s | 1 | 0 | 5900 | 2881 | 87% |
| math_totient_sum | mathematical-reasoning | yes | 99 A | 17.9s | 4 | 3 | 23436 | 1006 | 85% |
| math_binomial_mod | mathematical-reasoning | yes | 98 A | 33.2s | 4 | 3 | 24309 | 3713 | 84% |
| math_matrix_pow | mathematical-reasoning | yes | 99 A | 17.0s | 4 | 3 | 24400 | 1566 | 83% |
| math_mod_sqrt | mathematical-reasoning | yes | 98 A | 24.2s | 4 | 3 | 24867 | 2579 | 82% |
| math_convex_hull | mathematical-reasoning | yes | 95 A | 57.5s | 4 | 3 | 24814 | 3090 | 83% |
| math_partition_mod | mathematical-reasoning | yes | 99 A | 22.9s | 4 | 3 | 24148 | 2076 | 84% |

## Failures

### math_proof_inf_primes (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T151416Z-math_proof_inf_primes-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T151416Z-math_proof_inf_primes-r1/data/eix.sqlite3

```

