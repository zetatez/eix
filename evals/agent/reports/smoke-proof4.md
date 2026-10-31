# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: math_def_continuity, math_def_group, math_def_limit, math_proof_cantor, math_proof_induction_odds, math_proof_inf_primes, math_proof_sqrt2, math_theorem_rolle
- repeats: 3

## Summary

- score: **98.8** / 100
- pass: **24/24** (100%)
- axes: correctness 100, adherence 100, loops 100, tokens 100, time 100, cache 86
- mean duration: **5.8s**
- mean model steps: **1.00**
- mean tool calls: **0.00**
- prompt tokens: **131874**
- completion tokens: **16795**
- usage coverage: **100%** of model steps
- cache hit rate: **86.2%**

## Runs

| task                      | category               | pass | score | time  | steps | tools | prompt | completion | cache hit |
| ---                       | ---                    | ---  | ---   | ---:  | ---:  | ---:  | ---:   | ---:       | ---:      |
| math_proof_sqrt2          | mathematical-reasoning | yes  | 99 A  | 4.8s  | 1     | 0     | 5487   | 608        | 86%       |
| math_proof_sqrt2          | mathematical-reasoning | yes  | 99 A  | 4.6s  | 1     | 0     | 5487   | 543        | 86%       |
| math_proof_sqrt2          | mathematical-reasoning | yes  | 99 A  | 4.2s  | 1     | 0     | 5487   | 428        | 86%       |
| math_proof_inf_primes     | mathematical-reasoning | yes  | 99 A  | 6.0s  | 1     | 0     | 5496   | 739        | 86%       |
| math_proof_inf_primes     | mathematical-reasoning | yes  | 99 A  | 4.5s  | 1     | 0     | 5496   | 375        | 86%       |
| math_proof_inf_primes     | mathematical-reasoning | yes  | 99 A  | 3.8s  | 1     | 0     | 5496   | 358        | 86%       |
| math_proof_induction_odds | mathematical-reasoning | yes  | 99 A  | 3.3s  | 1     | 0     | 5508   | 316        | 86%       |
| math_proof_induction_odds | mathematical-reasoning | yes  | 99 A  | 5.2s  | 1     | 0     | 5508   | 526        | 86%       |
| math_proof_induction_odds | mathematical-reasoning | yes  | 99 A  | 4.0s  | 1     | 0     | 5508   | 523        | 86%       |
| math_def_limit            | mathematical-reasoning | yes  | 99 A  | 3.8s  | 1     | 0     | 5508   | 428        | 86%       |
| math_def_limit            | mathematical-reasoning | yes  | 99 A  | 5.8s  | 1     | 0     | 5508   | 421        | 86%       |
| math_def_limit            | mathematical-reasoning | yes  | 99 A  | 5.6s  | 1     | 0     | 5508   | 721        | 86%       |
| math_def_continuity       | mathematical-reasoning | yes  | 99 A  | 4.9s  | 1     | 0     | 5491   | 629        | 86%       |
| math_def_continuity       | mathematical-reasoning | yes  | 99 A  | 5.3s  | 1     | 0     | 5491   | 517        | 86%       |
| math_def_continuity       | mathematical-reasoning | yes  | 99 A  | 6.7s  | 1     | 0     | 5491   | 710        | 86%       |
| math_def_group            | mathematical-reasoning | yes  | 99 A  | 7.4s  | 1     | 0     | 5474   | 1038       | 87%       |
| math_def_group            | mathematical-reasoning | yes  | 99 A  | 7.2s  | 1     | 0     | 5474   | 1002       | 87%       |
| math_def_group            | mathematical-reasoning | yes  | 99 A  | 5.6s  | 1     | 0     | 5474   | 830        | 87%       |
| math_theorem_rolle        | mathematical-reasoning | yes  | 99 A  | 3.8s  | 1     | 0     | 5503   | 431        | 86%       |
| math_theorem_rolle        | mathematical-reasoning | yes  | 99 A  | 4.5s  | 1     | 0     | 5503   | 471        | 86%       |
| math_theorem_rolle        | mathematical-reasoning | yes  | 99 A  | 4.6s  | 1     | 0     | 5503   | 553        | 86%       |
| math_proof_cantor         | mathematical-reasoning | yes  | 99 A  | 13.2s | 1     | 0     | 5491   | 1863       | 86%       |
| math_proof_cantor         | mathematical-reasoning | yes  | 99 A  | 10.0s | 1     | 0     | 5491   | 1319       | 86%       |
| math_proof_cantor         | mathematical-reasoning | yes  | 99 A  | 11.4s | 1     | 0     | 5491   | 1446       | 86%       |

## Failures

None.
