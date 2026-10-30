# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, fix_pagination, merge_inventory
- repeats: 2

## Summary

- pass: **6/6** (100%)
- mean duration: **21.2s**
- mean model steps: **5.50**
- mean tool calls: **4.67**
- prompt tokens: **221674**
- completion tokens: **8721**
- usage coverage: **100%** of model steps
- cache hit rate: **63.5%**

## Runs

| task | pass | time | steps | tools | prompt | completion | cache hit |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| add_slugify | yes | 13.8s | 5 | 4 | 32017 | 818 | 70% |
| add_slugify | yes | 18.6s | 5 | 4 | 31830 | 848 | 52% |
| fix_pagination | yes | 16.8s | 4 | 3 | 25721 | 1155 | 42% |
| fix_pagination | yes | 17.3s | 6 | 5 | 39120 | 1111 | 73% |
| merge_inventory | yes | 29.2s | 6 | 6 | 43105 | 2933 | 73% |
| merge_inventory | yes | 31.4s | 7 | 6 | 49881 | 1856 | 62% |

## Failures

None.
