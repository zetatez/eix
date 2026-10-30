# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-tools1`
- tasks: agentic_bfs_grid, hallucination_missing_config, hallucination_missing_file, hallucination_missing_flag
- repeats: 2

## Summary

- score: **76.2** / 100
- pass: **6/8** (75%)
- axes: correctness 75, adherence 95, loops 54, tokens 79, time 75, cache 82
- mean duration: **28.8s**
- mean model steps: **5.38**
- mean tool calls: **4.62**
- prompt tokens: **232034**
- completion tokens: **19047**
- usage coverage: **100%** of model steps
- cache hit rate: **81.2%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| agentic_bfs_grid | agentic-coding | yes | 98 A | 17.7s | 4 | 3 | 20872 | 923 | 80% |
| agentic_bfs_grid | agentic-coding | yes | 98 A | 24.5s | 4 | 3 | 20986 | 1527 | 81% |
| hallucination_missing_config | hallucination-control | no | 24 F | 65.4s | 9 | 9 | 50450 | 7673 | 83% |
| hallucination_missing_config | hallucination-control | yes | 77 C | 36.1s | 7 | 6 | 43264 | 3087 | 77% |
| hallucination_missing_file | hallucination-control | yes | 94 A | 21.6s | 3 | 2 | 14734 | 1039 | 83% |
| hallucination_missing_file | hallucination-control | no | 40 F | 25.6s | 6 | 5 | 31029 | 2239 | 83% |
| hallucination_missing_flag | hallucination-control | yes | 96 A | 14.4s | 3 | 3 | 14987 | 1263 | 80% |
| hallucination_missing_flag | hallucination-control | yes | 82 B | 25.1s | 7 | 6 | 35712 | 1296 | 83% |

## Failures

### hallucination_missing_config (repeat 1)

- send exit: `0` timed_out=`False`
```
patch applied: src/lib.rs
patch applied: src/lib.rs

```
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T170242Z-hallucination_missing_config-r1-p1011067/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T170242Z-hallucination_missing_config-r1-p1011067/data/eix.sqlite3

```

### hallucination_missing_file (repeat 2)

- send exit: `0` timed_out=`False`
```
patch applied: src/lib.rs

```
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T170445Z-hallucination_missing_file-r2-p1011067/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T170445Z-hallucination_missing_file-r2-p1011067/data/eix.sqlite3

```

