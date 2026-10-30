# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-old`
- tasks: math_pythagorean, math_radix
- repeats: 2

## Summary

- score: **84.0** / 100
- pass: **4/4** (100%)
- axes: correctness 100, adherence 94, loops 62, tokens 61, time 65, cache 84
- mean duration: **52.6s**
- mean model steps: **8.25**
- mean tool calls: **7.50**
- prompt tokens: **245300**
- completion tokens: **14327**
- usage coverage: **100%** of model steps
- cache hit rate: **83.7%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| math_pythagorean | mathematical-reasoning | yes | 75 C | 69.2s | 10 | 10 | 82330 | 4373 | 81% |
| math_pythagorean | mathematical-reasoning | yes | 64 D | 98.7s | 14 | 13 | 108300 | 6997 | 86% |
| math_radix | mathematical-reasoning | yes | 99 A | 23.0s | 5 | 4 | 30720 | 1543 | 84% |
| math_radix | mathematical-reasoning | yes | 99 A | 19.5s | 4 | 3 | 23950 | 1414 | 84% |

## Failures

None.
