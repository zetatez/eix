# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: instruction_answer_format, instruction_api_shape, instruction_doc_comment, instruction_exact, instruction_exact_constant, instruction_format_output, instruction_no_files, instruction_no_mut, instruction_no_unsafe, instruction_range_check
- repeats: 1

## Summary

- score: **91.8** / 100
- pass: **9/10** (90%)
- axes: correctness 90, adherence 100, loops 94, tokens 99, time 99, cache 86
- mean duration: **13.1s**
- mean model steps: **3.70**
- mean tool calls: **2.70**
- prompt tokens: **215793**
- completion tokens: **4765**
- usage coverage: **100%** of model steps
- cache hit rate: **85.1%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| instruction_exact | instruction-following | yes | 98 A | 17.8s | 5 | 4 | 29933 | 765 | 85% |
| instruction_no_unsafe | instruction-following | yes | 99 A | 14.1s | 4 | 3 | 23169 | 519 | 85% |
| instruction_format_output | instruction-following | yes | 99 A | 17.3s | 5 | 4 | 29749 | 991 | 85% |
| instruction_api_shape | instruction-following | yes | 99 A | 14.8s | 4 | 3 | 23256 | 325 | 85% |
| instruction_answer_format | instruction-following | yes | 99 A | 2.2s | 1 | 0 | 5469 | 88 | 87% |
| instruction_no_files | instruction-following | no | 40 F | 2.9s | 1 | 0 | 5461 | 98 | 87% |
| instruction_exact_constant | instruction-following | yes | 96 A | 19.9s | 4 | 3 | 22955 | 408 | 85% |
| instruction_range_check | instruction-following | yes | 95 A | 18.1s | 5 | 4 | 29585 | 580 | 85% |
| instruction_doc_comment | instruction-following | yes | 97 A | 11.6s | 4 | 3 | 23067 | 386 | 85% |
| instruction_no_mut | instruction-following | yes | 97 A | 12.7s | 4 | 3 | 23149 | 605 | 85% |

## Failures

### instruction_no_files (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T155647Z-instruction_no_files-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T155647Z-instruction_no_files-r1/data/eix.sqlite3

```

