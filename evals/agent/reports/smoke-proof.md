# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: math_def_continuity, math_def_group, math_def_limit, math_proof_cantor, math_proof_induction_odds, math_proof_inf_primes, math_proof_sqrt2, math_theorem_rolle
- repeats: 1

## Summary

- score: **84.1** / 100
- pass: **6/8** (75%)
- axes: correctness 75, adherence 100, loops 100, tokens 100, time 99, cache 87
- mean duration: **7.1s**
- mean model steps: **1.00**
- mean tool calls: **0.00**
- prompt tokens: **47230**
- completion tokens: **5899**
- usage coverage: **100%** of model steps
- cache hit rate: **86.7%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| math_proof_sqrt2 | mathematical-reasoning | no | 40 F | 4.5s | 1 | 0 | 5896 | 523 | 87% |
| math_proof_inf_primes | mathematical-reasoning | yes | 99 A | 6.7s | 1 | 0 | 5905 | 505 | 87% |
| math_proof_induction_odds | mathematical-reasoning | yes | 99 A | 6.0s | 1 | 0 | 5917 | 404 | 87% |
| math_def_limit | mathematical-reasoning | yes | 99 A | 4.3s | 1 | 0 | 5917 | 415 | 87% |
| math_def_continuity | mathematical-reasoning | yes | 99 A | 5.1s | 1 | 0 | 5900 | 478 | 87% |
| math_def_group | mathematical-reasoning | yes | 99 A | 7.1s | 1 | 0 | 5883 | 970 | 87% |
| math_theorem_rolle | mathematical-reasoning | yes | 99 A | 4.6s | 1 | 0 | 5912 | 449 | 87% |
| math_proof_cantor | mathematical-reasoning | no | 40 F | 18.1s | 1 | 0 | 5900 | 2155 | 87% |

## Failures

### math_proof_sqrt2 (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T150917Z-math_proof_sqrt2-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T150917Z-math_proof_sqrt2-r1/data/eix.sqlite3

```

### math_proof_cantor (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T150956Z-math_proof_cantor-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T150956Z-math_proof_cantor-r1/data/eix.sqlite3

```

