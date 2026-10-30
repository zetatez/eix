# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-target/debug`
- tasks: add_slugify, fix_pagination, merge_inventory
- repeats: 2

## Summary

- pass: **6/6** (100%)
- mean duration: **22.0s**
- mean model steps: **5.00**
- mean tool calls: **4.50**
- prompt tokens: **200173**
- completion tokens: **9086**
- usage coverage: **100%** of model steps
- cache hit rate: **76.3%**

## Runs

| task | pass | time | steps | tools | prompt | completion | cache hit |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| add_slugify | yes | 17.1s | 5 | 4 | 31834 | 1067 | 70% |
| add_slugify | yes | 10.5s | 4 | 3 | 25005 | 600 | 85% |
| fix_pagination | yes | 26.8s | 5 | 4 | 32065 | 1414 | 68% |
| fix_pagination | yes | 15.2s | 5 | 4 | 32322 | 1025 | 86% |
| merge_inventory | yes | 16.1s | 4 | 4 | 27266 | 1137 | 84% |
| merge_inventory | yes | 46.5s | 7 | 8 | 51681 | 3843 | 71% |

## Failures

None.
