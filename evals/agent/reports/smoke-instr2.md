# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: instruction_no_files
- repeats: 3

## Summary

- score: **98.8** / 100
- pass: **3/3** (100%)
- axes: correctness 100, adherence 100, loops 100, tokens 100, time 100, cache 87
- mean duration: **2.8s**
- mean model steps: **1.00**
- mean tool calls: **0.00**
- prompt tokens: **16383**
- completion tokens: **277**
- usage coverage: **100%** of model steps
- cache hit rate: **86.7%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| instruction_no_files | instruction-following | yes | 99 A | 3.9s | 1 | 0 | 5461 | 128 | 87% |
| instruction_no_files | instruction-following | yes | 99 A | 2.6s | 1 | 0 | 5461 | 91 | 87% |
| instruction_no_files | instruction-following | yes | 99 A | 2.0s | 1 | 0 | 5461 | 58 | 87% |

## Failures

None.
