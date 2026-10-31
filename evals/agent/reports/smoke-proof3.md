# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: math_def_continuity, math_def_group, math_def_limit, math_proof_cantor, math_proof_induction_odds, math_proof_inf_primes, math_proof_sqrt2, math_theorem_rolle
- repeats: 3

## Summary

- score: **96.4** / 100
- pass: **23/24** (96%)
- axes: correctness 96, adherence 100, loops 100, tokens 100, time 100, cache 86
- mean duration: **5.5s**
- mean model steps: **1.00**
- mean tool calls: **0.00**
- prompt tokens: **131874**
- completion tokens: **16546**
- usage coverage: **100%** of model steps
- cache hit rate: **86.2%**

## Runs

| task                      | category               | pass | score | time  | steps | tools | prompt | completion | cache hit |
| ---                       | ---                    | ---  | ---   | ---:  | ---:  | ---:  | ---:   | ---:       | ---:      |
| math_proof_sqrt2          | mathematical-reasoning | yes  | 99 A  | 5.2s  | 1     | 0     | 5487   | 655        | 86%       |
| math_proof_sqrt2          | mathematical-reasoning | yes  | 99 A  | 4.3s  | 1     | 0     | 5487   | 524        | 86%       |
| math_proof_sqrt2          | mathematical-reasoning | yes  | 99 A  | 5.7s  | 1     | 0     | 5487   | 841        | 86%       |
| math_proof_inf_primes     | mathematical-reasoning | yes  | 99 A  | 5.2s  | 1     | 0     | 5496   | 497        | 86%       |
| math_proof_inf_primes     | mathematical-reasoning | yes  | 99 A  | 6.8s  | 1     | 0     | 5496   | 748        | 86%       |
| math_proof_inf_primes     | mathematical-reasoning | yes  | 99 A  | 4.8s  | 1     | 0     | 5496   | 354        | 86%       |
| math_proof_induction_odds | mathematical-reasoning | no   | 40 F  | 6.2s  | 1     | 0     | 5508   | 522        | 86%       |
| math_proof_induction_odds | mathematical-reasoning | yes  | 99 A  | 3.6s  | 1     | 0     | 5508   | 472        | 86%       |
| math_proof_induction_odds | mathematical-reasoning | yes  | 99 A  | 4.9s  | 1     | 0     | 5508   | 412        | 86%       |
| math_def_limit            | mathematical-reasoning | yes  | 99 A  | 4.8s  | 1     | 0     | 5508   | 531        | 86%       |
| math_def_limit            | mathematical-reasoning | yes  | 99 A  | 4.1s  | 1     | 0     | 5508   | 536        | 86%       |
| math_def_limit            | mathematical-reasoning | yes  | 99 A  | 3.2s  | 1     | 0     | 5508   | 328        | 86%       |
| math_def_continuity       | mathematical-reasoning | yes  | 99 A  | 5.0s  | 1     | 0     | 5491   | 584        | 86%       |
| math_def_continuity       | mathematical-reasoning | yes  | 99 A  | 4.3s  | 1     | 0     | 5491   | 631        | 86%       |
| math_def_continuity       | mathematical-reasoning | yes  | 99 A  | 5.9s  | 1     | 0     | 5491   | 628        | 86%       |
| math_def_group            | mathematical-reasoning | yes  | 99 A  | 6.0s  | 1     | 0     | 5474   | 999        | 87%       |
| math_def_group            | mathematical-reasoning | yes  | 99 A  | 4.8s  | 1     | 0     | 5474   | 620        | 87%       |
| math_def_group            | mathematical-reasoning | yes  | 99 A  | 5.6s  | 1     | 0     | 5474   | 970        | 87%       |
| math_theorem_rolle        | mathematical-reasoning | yes  | 99 A  | 3.7s  | 1     | 0     | 5503   | 505        | 86%       |
| math_theorem_rolle        | mathematical-reasoning | yes  | 99 A  | 3.4s  | 1     | 0     | 5503   | 476        | 86%       |
| math_theorem_rolle        | mathematical-reasoning | yes  | 99 A  | 3.9s  | 1     | 0     | 5503   | 464        | 86%       |
| math_proof_cantor         | mathematical-reasoning | yes  | 99 A  | 8.1s  | 1     | 0     | 5491   | 959        | 86%       |
| math_proof_cantor         | mathematical-reasoning | yes  | 99 A  | 14.0s | 1     | 0     | 5491   | 2053       | 86%       |
| math_proof_cantor         | mathematical-reasoning | yes  | 99 A  | 8.5s  | 1     | 0     | 5491   | 1237       | 86%       |

## Failures

### math_proof_induction_odds (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T155036Z-math_proof_induction_odds-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T155036Z-math_proof_induction_odds-r1/data/eix.sqlite3

```

