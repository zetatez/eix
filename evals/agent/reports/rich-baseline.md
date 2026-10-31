# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, counter_saturate, fix_pagination, hallucination_missing_api, instruction_exact, math_combinatorics, math_rational, merge_inventory, planning_pipeline, refactor_dupes, science_kinematics, state_machine, validate_config
- repeats: 2

## Summary

- score: **91.7** / 100
- pass: **26/26** (100%)
- axes: correctness 100, adherence 99, planning 100, loops 82, tokens 81, time 82, cache 82
- mean duration: **36.6s**
- mean model steps: **6.15**
- mean tool calls: **5.38**
- prompt tokens: **1200704**
- completion tokens: **91211**
- usage coverage: **100%** of model steps
- cache hit rate: **81.3%**

## Runs

| task                      | pass | score | time   | steps | tools | prompt | completion | cache hit |
| ---                       | ---  | ---:  | ---:   | ---:  | ---:  | ---:   | ---:       | ---:      |
| add_slugify               | yes  | 99 A  | 12.6s  | 4     | 3     | 25023  | 590        | 85%       |
| add_slugify               | yes  | 99 A  | 17.8s  | 4     | 3     | 25018  | 1034       | 84%       |
| counter_saturate          | yes  | 99 A  | 12.6s  | 4     | 3     | 25760  | 554        | 85%       |
| counter_saturate          | yes  | 95 A  | 18.6s  | 5     | 4     | 32421  | 1022       | 85%       |
| fix_pagination            | yes  | 99 A  | 10.9s  | 4     | 3     | 25504  | 647        | 85%       |
| fix_pagination            | yes  | 99 A  | 15.7s  | 4     | 3     | 25561  | 903        | 83%       |
| hallucination_missing_api | yes  | 90 A  | 15.1s  | 4     | 3     | 24751  | 891        | 65%       |
| hallucination_missing_api | yes  | 84 B  | 21.9s  | 6     | 5     | 38912  | 1348       | 84%       |
| instruction_exact         | yes  | 88 B  | 26.3s  | 7     | 6     | 46375  | 1786       | 73%       |
| instruction_exact         | yes  | 92 A  | 22.7s  | 6     | 5     | 38731  | 1336       | 87%       |
| math_combinatorics        | yes  | 61 D  | 189.4s | 20    | 19    | 211027 | 21454      | 75%       |
| math_combinatorics        | yes  | 88 B  | 65.1s  | 7     | 6     | 48875  | 7807       | 83%       |
| math_rational             | yes  | 66 D  | 141.0s | 11    | 10    | 100136 | 19078      | 78%       |
| math_rational             | yes  | 85 B  | 86.6s  | 7     | 6     | 51952  | 9569       | 82%       |
| merge_inventory           | yes  | 99 A  | 17.5s  | 4     | 3     | 26810  | 1228       | 84%       |
| merge_inventory           | yes  | 97 A  | 44.9s  | 4     | 3     | 26721  | 1014       | 84%       |
| planning_pipeline         | yes  | 95 A  | 19.4s  | 7     | 7     | 51168  | 1451       | 84%       |
| planning_pipeline         | yes  | 98 A  | 17.8s  | 6     | 7     | 41021  | 811        | 83%       |
| refactor_dupes            | yes  | 63 D  | 91.7s  | 14    | 14    | 122134 | 10569      | 85%       |
| refactor_dupes            | yes  | 98 A  | 20.8s  | 4     | 3     | 26396  | 2493       | 81%       |
| science_kinematics        | yes  | 99 A  | 11.9s  | 4     | 3     | 26997  | 874        | 84%       |
| science_kinematics        | yes  | 99 A  | 12.1s  | 4     | 3     | 26301  | 860        | 84%       |
| state_machine             | yes  | 99 A  | 10.6s  | 4     | 3     | 25652  | 398        | 85%       |
| state_machine             | yes  | 98 A  | 14.5s  | 5     | 4     | 32704  | 675        | 83%       |
| validate_config           | yes  | 97 A  | 21.4s  | 6     | 6     | 41964  | 1988       | 86%       |
| validate_config           | yes  | 99 A  | 13.2s  | 5     | 5     | 32790  | 831        | 85%       |

## Failures

None.
