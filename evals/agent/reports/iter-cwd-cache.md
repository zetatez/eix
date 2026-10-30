# eix agent eval

- model: `deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, fix_pagination, merge_inventory
- repeats: 2

## Summary

- pass: **6/6** (100%)
- mean duration: **32.0s**
- mean model steps: **7.00**
- mean tool calls: **6.17**
- prompt tokens: **293695**
- completion tokens: **11878**
- usage coverage: **100%** of model steps
- cache hit rate: **77.0%**

## Runs

| task | pass | time | steps | tools | prompt | completion | cache hit |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| add_slugify | yes | 14.6s | 4 | 3 | 24707 | 523 | 43% |
| add_slugify | yes | 17.6s | 4 | 3 | 24823 | 655 | 84% |
| fix_pagination | yes | 32.3s | 8 | 7 | 55477 | 1897 | 74% |
| fix_pagination | yes | 33.3s | 8 | 7 | 55282 | 2631 | 86% |
| merge_inventory | yes | 48.4s | 9 | 8 | 66766 | 2910 | 76% |
| merge_inventory | yes | 46.0s | 9 | 9 | 66640 | 3262 | 84% |

## Failures

None.
