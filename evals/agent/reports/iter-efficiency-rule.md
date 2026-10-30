# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, fix_pagination, merge_inventory
- repeats: 2

## Summary

- pass: **6/6** (100%)
- mean duration: **22.4s**
- mean model steps: **5.17**
- mean tool calls: **4.33**
- prompt tokens: **209320**
- completion tokens: **9596**
- usage coverage: **100%** of model steps
- cache hit rate: **76.8%**

## Runs

| task | pass | time | steps | tools | prompt | completion | cache hit |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| add_slugify | yes | 11.9s | 4 | 3 | 25048 | 728 | 66% |
| add_slugify | yes | 12.5s | 4 | 3 | 25001 | 596 | 86% |
| fix_pagination | yes | 17.1s | 5 | 4 | 32674 | 843 | 66% |
| fix_pagination | yes | 17.2s | 4 | 3 | 25335 | 663 | 84% |
| merge_inventory | yes | 35.4s | 7 | 6 | 50270 | 3547 | 86% |
| merge_inventory | yes | 40.0s | 7 | 7 | 50992 | 3219 | 72% |

## Failures

None.
