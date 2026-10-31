# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, counter_saturate, fix_pagination, merge_inventory, refactor_dupes, state_machine, validate_config
- repeats: 2

## Summary

- score: **93.5** / 100
- pass: **14/14** (100%)
- axes: correctness 100, discipline 98, efficiency 89, cache 77
- mean duration: **25.1s**
- mean model steps: **5.43**
- mean tool calls: **4.79**
- prompt tokens: **523494**
- completion tokens: **28246**
- usage coverage: **100%** of model steps
- cache hit rate: **80.8%**

## Runs

| task             | pass | score | time  | steps | tools | prompt | completion | cache hit |
| ---              | ---  | ---:  | ---:  | ---:  | ---:  | ---:   | ---:       | ---:      |
| add_slugify      | yes  | 90 B  | 15.1s | 5     | 4     | 31644  | 1005       | 71%       |
| add_slugify      | yes  | 98 A  | 12.5s | 4     | 3     | 25061  | 599        | 85%       |
| counter_saturate | yes  | 98 A  | 9.8s  | 4     | 3     | 25775  | 534        | 85%       |
| counter_saturate | yes  | 98 A  | 12.2s | 4     | 3     | 26156  | 600        | 85%       |
| fix_pagination   | yes  | 95 A  | 14.5s | 5     | 4     | 32419  | 993        | 86%       |
| fix_pagination   | yes  | 95 A  | 15.9s | 5     | 4     | 32416  | 1024       | 86%       |
| merge_inventory  | yes  | 80 C  | 76.0s | 10    | 12    | 78487  | 8768       | 80%       |
| merge_inventory  | yes  | 98 A  | 23.2s | 5     | 4     | 33755  | 1806       | 85%       |
| refactor_dupes   | yes  | 95 A  | 35.5s | 4     | 3     | 26296  | 2133       | 66%       |
| refactor_dupes   | yes  | 81 B  | 52.3s | 9     | 8     | 68705  | 5401       | 85%       |
| state_machine    | yes  | 92 A  | 13.4s | 5     | 4     | 33339  | 651        | 69%       |
| state_machine    | yes  | 97 A  | 14.9s | 5     | 4     | 32540  | 632        | 84%       |
| validate_config  | yes  | 97 A  | 28.7s | 5     | 5     | 33844  | 2182       | 82%       |
| validate_config  | yes  | 96 A  | 27.1s | 6     | 6     | 43057  | 1918       | 80%       |

## Failures

None.
