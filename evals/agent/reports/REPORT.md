# Agent evaluation report

This is the summary of the four prompt/tool iterations measured with the
127-task suite. It covers what changed, what it cost, what it did not fix, and
how to reproduce the numbers. `SUMMARY.md` is the detailed log; `README.md`
documents the suite and the harness.

## Executive summary

- The 127-task suite is stable enough to guide changes, but a single run is
  not: the same binary varies by 1-3 passes and by ~10-15% tokens.
- Prompt discipline (iter1) removed about 0.4 steps and 3k tokens per task with
  no pass loss.
- Deferring tool schemas (iter2, iter3) cut the first request from 5535 to 4371
  tokens (-21%) with no extra `tool_search` calls and no behavior change. The
  saved tokens are cached prefix, so the cash effect is small; the real win is
  context headroom and a smaller choice space.
- The remaining cost is not schemas or tool output. It is steps, and inside
  each step the uncached tail plus output tokens, 63-65% of which are
  reasoning.
- The behavior line (iter4, "a green run ends the task") halved `git` calls and
  cut scratch writes and shell calls, but did not move steps, tokens, or cost
  outside noise. It is worth keeping for hygiene, not as a performance fix.
- Two full arms were lost to `/tmp` filling up; that failure mode is documented
  below so it is not mistaken for model variance again.

## Method

- Model: `deepseek/deepseek-v4.1-flash` through the local gateway.
- Harness: Python standard library, driving the real `eix`/`eixd` binaries.
  Each task gets its own data dir, Unix socket, daemon, git repo, and work
  tree. Hidden tests are injected after the agent stops and removed afterwards.
- Mode: `full` (no approval prompts), so the measured loop is the intended one.
- Scoring: correctness 35, adherence 15, planning 8, loops 12, tokens 12, time
  10, cache 8. Missing axes are dropped and the weights renormalised. A failed
  run is capped at 40; a delta under 2 points is noise.
- Parallelism: tasks are split into shards and each shard is a separate
  `run_eval.py` process; `/tmp/par_eval.sh` merges the shard reports. Three
  shards are safe with ~8G free under `/tmp`; four is where disk pressure
  starts.
- Repeats: one run per arm for baseline/iter1/iter2; three clean runs per arm
  for iter3/iter4, which is what the cost axes are compared on.

The suite: 40 agentic coding, 51 math (including eight no-code proofs and six
multi-step algorithms), 10 hallucination control, 10 instruction following, 8
science, 8 planning. Every code task is verified twice before entering: the
untouched template must fail its hidden tests and a reference solution must
pass.

## Iterations

| arm | change | first request | pass | steps | tokens/task | USD/task |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| baseline | old binary | 5535 | 125/127 | 4.97 | 33.5k | 0.0022 |
| iter1 | prompt: after a patch, do not re-read, `git diff`/`git status`, or rebuild; run the workspace tests once; stay inside the workspace | 5550 | 125/127 | 4.55 | 30.4k | 0.0020 |
| iter2 | Full mode advertises only tools that can fire; web/image/LSP move behind `tool_search` | 4691 | 123/127 | 4.56 | 26.9k | 0.0020 |
| iter3 | sub-agents, memory and context use deferred too; exec descriptions trimmed | **4371** | 126/127 | 4.50 | 24.4k | 0.0018 |
| iter4 | "a green test run ends the task" | 4394 | 124/127 | 4.37 | 23.9k | 0.0018 |

iter3 and iter4 numbers are the last clean run; over three runs each:

| arm | pass/run | score | steps | tokens/task | USD/task | git calls | scratch | exec calls |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| iter3 | 125.3/127 | 95.5 | 4.46 | 24.1k | 0.00180 | 0.18 | 0.03 | 2.43 |
| iter4 | 124.7/127 | 95.2 | 4.41 | 24.0k | 0.00180 | **0.09** | **0.02** | **2.30** |

The pass difference is one checker false negative in one iter4 run, not a
behavior change. After the final checker repairs both arms are effectively at
~126/127; the persistent failure is `math_determinant`, where the template's
Sarrus expansion is wrong but still passes its visible test, and the model
keeps it while adding checked arithmetic. That is the intended trap.

### Tools

`exec_command` and `apply_patch` are ~99% of calls in every arm. Deferring
optional built-ins added no `tool_search` calls in normal work, so the schema
trim did not cost a step. Full-mode schemas are now ~1.4k tokens, down from
~2.4k before iter2.

### Why the tool trimming did not cut the bill

The first request is ~4.4k prompt tokens, of which 3584 hit the provider's
shared prefix; only ~800 are uncached. Cache reads cost 2% of an input token,
so removing cached schema tokens changes the token count but barely the price.
The cost split is about 37% uncached input, 37% reasoning output, 26% visible
output.

Per-step measurement (event store kept for six heavy tasks) shows 800-1500
uncached tokens per step, roughly one cache block of tail plus new content, and
tool results of 100-450 tokens. The 4000-token `context.tool_output_tokens` cap
never fired, so lowering it would not help this workload and would force
follow-up reads on genuinely large output.

The levers left are step count and reasoning length.

## Variance and infrastructure

Three repeats over the 13 heaviest tasks:

- median per-task step SD **1.25**, median token CV **13.9%**;
- `math_isqrt` took 4 steps in one run and 18 in another;
- `planning_multifile` is consistently long (12 ± 1.6 steps, ~99k tokens).

`/tmp` is a 16G tmpfs. Two full arms were lost to `ENOSPC`: daemons died with
`daemon closed the connection` after 0-1 steps, `eix new` failed, and some runs
reported `Disk quota exceeded`. Those runs are excluded. Keep ~8G free, use
three shards, and delete `$CARGO_TARGET_DIR/debug/incremental` before a long
comparison.

Provider failures are separate: `upstream stream interrupted` shows up in
roughly one run per hundred and is retried by re-running the task.

## Benchmark quality

The suite started at 81 tasks and grew to 127 because the shallow math tasks
were not measuring reasoning. The additions and the checker repairs are
documented in `SUMMARY.md`. The important rule: a correct answer must not fail
for notation or phrasing. Checks now flatten LaTeX/Unicode and accept common
negative phrasings ("premise doesn't hold", "no reference", "nothing to fix",
"no change was needed", "contradicting the assumption"). Proof tasks still
require the essential steps.

## What did not work or is not worth doing

- Hiding more tools: the remaining schemas are cached prefix; another round
  would risk a `tool_search` step for no cash gain.
- Lowering the tool-output cap: it never fires on this workload.
- Trusting a single full-suite pass or a single task's step count: repeat or
  compare medians.
- Treating the mean step count as a proxy for the long tail: the tail is
  individual tasks that keep verifying after the tests pass, and iter4 only
  trimmed the waste calls around it.

## Next

1. Target the tail, not the average: a stop condition for long tasks, or a
   budget-aware reminder after the first green run.
2. Try to cut reasoning tokens (a stopping-condition line or a lower reasoning
   variant) and gate it on the math and proof pass rate staying flat.
3. Add at least three repeats to the arm protocol by default; one run cannot
   separate a 0.1-step change from noise.
4. Make the daemon log a reason when it exits instead of closing the socket
   silently, so an infrastructure failure is distinguishable from a bad run.

## Reproduce

```sh
# build the binary under test
cargo build -p eixd --bins

# one arm, four shards, merge the result
/tmp/par_eval.sh iter5 /path/to/bin 3

# compare arms
python3 evals/agent/compare.py evals/agent/reports/iter3.json \
  evals/agent/reports/iter4.json
```

Reports for every arm are in `evals/agent/reports/`; shard inputs are ignored
by git and only the merged arm reports are tracked.
