# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: math_proof_inf_primes
- repeats: 1

## Summary

- score: **98.8** / 100
- pass: **1/1** (100%)
- axes: correctness 100, adherence 100, loops 100, tokens 100, time 100, cache 86
- mean duration: **6.9s**
- mean model steps: **1.00**
- mean tool calls: **0.00**
- prompt tokens: **5496**
- completion tokens: **709**
- usage coverage: **100%** of model steps
- cache hit rate: **86.2%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| math_proof_inf_primes | mathematical-reasoning | yes | 99 A | 6.9s | 1 | 0 | 5496 | 709 | 86% |

## Failures

None.
