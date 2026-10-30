# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, counter_saturate, fix_pagination, merge_inventory, refactor_dupes, state_machine, validate_config
- repeats: 1

## Summary

- score: **94.8** / 100
- pass: **7/7** (100%)
- axes: correctness 100, discipline 100, efficiency 94, cache 76
- mean duration: **21.9s**
- mean model steps: **4.86**
- mean tool calls: **4.29**
- prompt tokens: **226541**
- completion tokens: **10444**
- usage coverage: **100%** of model steps
- cache hit rate: **76.6%**

## Runs

| task | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| add_slugify | yes | 95 A | 16.7s | 5 | 4 | 31607 | 1097 | 86% |
| counter_saturate | yes | 93 A | 19.8s | 5 | 4 | 33375 | 640 | 68% |
| fix_pagination | yes | 98 A | 10.7s | 4 | 3 | 25439 | 581 | 83% |
| merge_inventory | yes | 98 A | 29.2s | 5 | 7 | 34882 | 2615 | 81% |
| refactor_dupes | yes | 91 A | 43.8s | 6 | 5 | 42508 | 4422 | 70% |
| state_machine | yes | 92 A | 21.0s | 5 | 4 | 32033 | 591 | 68% |
| validate_config | yes | 98 A | 11.8s | 4 | 3 | 26697 | 498 | 85% |

## Failures

None.
