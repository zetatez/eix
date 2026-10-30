# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, counter_saturate, fix_pagination, merge_inventory, refactor_dupes, state_machine, validate_config
- repeats: 2

## Summary

- score: **93.8** / 100
- pass: **14/14** (100%)
- axes: correctness 100, discipline 96, efficiency 86, cache 85
- mean duration: **27.1s**
- mean model steps: **5.79**
- mean tool calls: **5.21**
- prompt tokens: **600028**
- completion tokens: **33996**
- usage coverage: **100%** of model steps
- cache hit rate: **83.4%**

## Runs

| task | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| add_slugify | yes | 98 A | 13.8s | 4 | 3 | 24913 | 569 | 85% |
| add_slugify | yes | 89 B | 22.6s | 6 | 5 | 38632 | 1312 | 86% |
| counter_saturate | yes | 95 A | 17.1s | 5 | 4 | 32982 | 710 | 86% |
| counter_saturate | yes | 98 A | 11.5s | 4 | 3 | 25287 | 664 | 86% |
| fix_pagination | yes | 95 A | 17.1s | 5 | 4 | 32053 | 820 | 85% |
| fix_pagination | yes | 98 A | 13.6s | 4 | 3 | 25473 | 754 | 85% |
| merge_inventory | yes | 97 A | 36.3s | 5 | 5 | 34742 | 2060 | 81% |
| merge_inventory | yes | 90 B | 40.1s | 8 | 10 | 60229 | 3625 | 81% |
| refactor_dupes | yes | 98 A | 23.2s | 4 | 3 | 26322 | 2623 | 84% |
| refactor_dupes | yes | 96 A | 23.2s | 5 | 4 | 33931 | 2724 | 80% |
| state_machine | yes | 98 A | 12.1s | 5 | 4 | 33099 | 660 | 85% |
| state_machine | yes | 98 A | 11.3s | 4 | 3 | 25448 | 543 | 84% |
| validate_config | yes | 92 A | 35.4s | 7 | 8 | 51520 | 4551 | 85% |
| validate_config | yes | 72 C | 102.2s | 15 | 14 | 155397 | 12381 | 82% |

## Failures

None.
