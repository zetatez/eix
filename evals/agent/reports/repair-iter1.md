# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: agentic_knapsack, hallucination_missing_arg
- repeats: 2

## Summary

- score: **57.1** / 100
- pass: **2/4** (50%)
- axes: correctness 50, adherence 100, loops 71, tokens 73, time 50, cache 84
- mean duration: **105.5s**
- mean model steps: **7.50**
- mean tool calls: **6.50**
- prompt tokens: **302656**
- completion tokens: **41140**
- usage coverage: **100%** of model steps
- cache hit rate: **84.4%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| agentic_knapsack | agentic-coding | yes | 62 D | 276.5s | 20 | 19 | 241422 | 33093 | 85% |
| agentic_knapsack | agentic-coding | yes | 87 B | 129.6s | 5 | 4 | 33072 | 7689 | 79% |
| hallucination_missing_arg | hallucination-control | no | 40 F | 10.1s | 3 | 2 | 16951 | 224 | 85% |
| hallucination_missing_arg | hallucination-control | no | 40 F | 5.7s | 2 | 1 | 11211 | 134 | 86% |

## Failures

### hallucination_missing_arg (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T170845Z-hallucination_missing_arg-r1-p1011066/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T170845Z-hallucination_missing_arg-r1-p1011066/data/eix.sqlite3

```

### hallucination_missing_arg (repeat 2)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T170856Z-hallucination_missing_arg-r2-p1011066/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T170856Z-hallucination_missing_arg-r2-p1011066/data/eix.sqlite3

```

