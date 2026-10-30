# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: math_def_continuity, math_def_group, math_def_limit, math_proof_cantor, math_proof_induction_odds, math_proof_inf_primes, math_proof_sqrt2, math_theorem_rolle
- repeats: 2

## Summary

- score: **80.4** / 100
- pass: **11/16** (69%)
- axes: correctness 69, adherence 100, loops 100, tokens 100, time 99, cache 86
- mean duration: **6.6s**
- mean model steps: **1.00**
- mean tool calls: **0.00**
- prompt tokens: **87916**
- completion tokens: **13803**
- usage coverage: **100%** of model steps
- cache hit rate: **86.2%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| math_proof_induction_odds | mathematical-reasoning | yes | 99 A | 5.7s | 1 | 0 | 5508 | 418 | 86% |
| math_proof_induction_odds | mathematical-reasoning | yes | 99 A | 3.0s | 1 | 0 | 5508 | 382 | 86% |
| math_proof_sqrt2 | mathematical-reasoning | yes | 99 A | 5.7s | 1 | 0 | 5487 | 678 | 86% |
| math_proof_sqrt2 | mathematical-reasoning | yes | 99 A | 7.0s | 1 | 0 | 5487 | 844 | 86% |
| math_proof_inf_primes | mathematical-reasoning | yes | 99 A | 5.5s | 1 | 0 | 5496 | 828 | 86% |
| math_proof_inf_primes | mathematical-reasoning | yes | 99 A | 3.9s | 1 | 0 | 5496 | 366 | 86% |
| math_def_limit | mathematical-reasoning | no | 40 F | 3.4s | 1 | 0 | 5508 | 329 | 86% |
| math_def_limit | mathematical-reasoning | no | 40 F | 4.2s | 1 | 0 | 5508 | 408 | 86% |
| math_def_continuity | mathematical-reasoning | yes | 99 A | 6.2s | 1 | 0 | 5491 | 940 | 86% |
| math_def_continuity | mathematical-reasoning | no | 40 F | 4.1s | 1 | 0 | 5491 | 561 | 86% |
| math_def_group | mathematical-reasoning | yes | 99 A | 5.9s | 1 | 0 | 5474 | 639 | 87% |
| math_def_group | mathematical-reasoning | yes | 99 A | 5.5s | 1 | 0 | 5474 | 794 | 87% |
| math_theorem_rolle | mathematical-reasoning | yes | 99 A | 3.9s | 1 | 0 | 5503 | 585 | 86% |
| math_theorem_rolle | mathematical-reasoning | yes | 99 A | 3.6s | 1 | 0 | 5503 | 416 | 86% |
| math_proof_cantor | mathematical-reasoning | no | 40 F | 23.7s | 1 | 0 | 5491 | 3503 | 86% |
| math_proof_cantor | mathematical-reasoning | no | 40 F | 14.0s | 1 | 0 | 5491 | 2112 | 86% |

## Failures

### math_def_limit (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T154807Z-math_def_limit-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T154807Z-math_def_limit-r1/data/eix.sqlite3

```

### math_def_limit (repeat 2)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T154811Z-math_def_limit-r2/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T154811Z-math_def_limit-r2/data/eix.sqlite3

```

### math_def_continuity (repeat 2)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T154821Z-math_def_continuity-r2/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T154821Z-math_def_continuity-r2/data/eix.sqlite3

```

### math_proof_cantor (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T154845Z-math_proof_cantor-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T154845Z-math_proof_cantor-r1/data/eix.sqlite3

```

### math_proof_cantor (repeat 2)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T154908Z-math_proof_cantor-r2/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T154908Z-math_proof_cantor-r2/data/eix.sqlite3

```

