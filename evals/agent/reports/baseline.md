# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, fix_pagination, merge_inventory
- repeats: 1

## Summary

- pass: **3/3** (100%)
- mean duration: **25.8s**
- mean model steps: **5.67**
- mean tool calls: **4.67**
- prompt tokens: **117054**
- completion tokens: **4502**
- usage coverage: **100%** of model steps
- cache hit rate: **62.2%**

## Runs

| task | pass | time | steps | tools | prompt | completion | cache hit |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| add_slugify | yes | 18.7s | 4 | 3 | 24997 | 883 | 43% |
| fix_pagination | yes | 14.6s | 4 | 3 | 25341 | 972 | 65% |
| merge_inventory | yes | 44.1s | 9 | 8 | 66716 | 2647 | 68% |

## Failures

None.
