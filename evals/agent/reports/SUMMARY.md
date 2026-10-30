# Iteration log

Model: `deepseek/deepseek-v4.1-flash` through the local gateway. Tasks run in
full mode, hidden tests injected only after the agent stops. Pass rate is the
gate; efficiency is only compared between runs that keep it at 100%.

## Scoring

Each run is scored 0-100 on seven axes (`score.py`):

| axis | weight | components |
| --- | ---: | --- |
| correctness | 35 | hidden pass + correctness checks |
| adherence | 15 | clean diff (40%), no scratch writes (30%), adherence checks (30%) |
| planning | 8 | `update_plan` used and completed (planning tasks only) |
| loops | 12 | `model_steps` against the task budget |
| tokens | 12 | prompt + completion tokens against the task budget |
| time | 10 | wall seconds against the task budget |
| cache | 8 | overall hit rate (70%), first-step hit rate (30%) |

Budget axes map linearly from the task's `[target, worst]` pair. `planning` is
dropped and the weights renormalised on non-planning tasks and on old reports.

A failed run is capped at 40. A delta under 2 points is labelled noise by
`compare.py`; a lost passing repeat is a regression that fails the comparison.

## Baselines

### 3-task suite (pre-scoring, kept for history)

| run | pass | mean time | steps | tools | tokens | cache hit |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `baseline2` | 6/6 | 21.2s | 5.50 | 4.67 | 230,395 | 63.5% |
| `iter-final-prompt` | 6/6 | 22.0s | 5.00 | 4.50 | 209,259 | 76.3% |

### 7-task suite (scored)

`score-baseline2`: 14/14 pass, mean score **93.8**.

| task | pass | score | steps (mean) |
| --- | --- | ---: | ---: |
| add_slugify | 2/2 | 93.3 | 5.0 |
| counter_saturate | 2/2 | 96.6 | 4.5 |
| fix_pagination | 2/2 | 96.5 | 4.5 |
| merge_inventory | 2/2 | 93.6 | 6.5 |
| refactor_dupes | 2/2 | 97.0 | 4.5 |
| state_machine | 2/2 | 97.6 | 4.5 |
| validate_config | 2/2 | 81.9 | 11.0 |

Axes: correctness 100, discipline 96, efficiency 86, cache 85.

The two outliers that year the mean:

- `validate_config` run 2 spent 15 steps and 168k tokens. It had the correct
  patch by step 5, then searched `/tmp`, `/home/shiyi` and `/` for the expected
  error payloads. That is a spec-ambiguity cost paid as filesystem probing.
- `merge_inventory` run 2 built a scratch crate under `mktemp -d` with a path
  dependency on the workspace to test edge cases, adding 2 steps and ~20k
  tokens.

### 13-task benchmark (scored, pre-checks)

`rich-baseline`: 26/26 pass, mean score **91.7**.

Axes: correctness 100, adherence 99, planning 100, loops 82, tokens 81,
time 82, cache 82. Categories: agentic 95.6, hallucination 86.9, instruction
89.9, math 75.2, science 98.7, planning 96.3.

Math was the weak category, driven by `math_combinatorics` (20 steps, 189s,
232k tokens) and `math_rational` (11 steps, 141s, 119k tokens). Both spent most
of their budget on scratch probes instead of editing the crate. This run used
the old scratch accounting, so it is not directly comparable to later runs.

### 127-task suite (scored, multi-category)

All arms run all 127 tasks, 1 repeat, in three parallel shards. `iter3` and
`iter4` were each run twice; only runs with enough free space under `/tmp` are
listed (see Disk, below).

| arm | what changed | pass | score | steps | secs | tokens/task | cache | first request |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `baseline` | old binary | 125/127 | 90.5 | 4.97 | 55.1 | 33.5k | 82.9% | 5535 |
| `iter1` | prompt: no re-read/re-diff/re-verify | 125/127 | 91.7 | 4.55 | 53.8 | 30.4k | 82.8% | 5550 |
| `iter2` | + Full mode advertises only usable tools; optional built-ins deferred | 123/127 | 91.4 | 4.56 | 51.7 | 26.9k | 80.2% | 4691 |
| `iter3` | + sub-agents, memory, context use deferred; exec descriptions trimmed | 126/127 | 95.7 | 4.50 | 21.8 | 24.4k | 78.8% | **4371** |
| `iter4` | + "a green run ends the task" | 124/127 | 94.9 | 4.37 | 20.3 | 23.9k | 77.9% | 4394 |

`iter3` and `iter4` were each run three times on the final task set. Means:

| arm | pass/run | score | steps | tokens/task | USD/task | git calls | scratch | exec calls |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `iter3` | 125.3/127 | 95.5 | 4.46 | 24.1k | 0.00180 | 0.18 | 0.03 | 2.43 |
| `iter4` | 124.7/127 | 95.2 | 4.41 | 24.0k | 0.00180 | **0.09** | **0.02** | **2.30** |

The pass gap is one checker false negative in one `iter4` run (see Checker
repairs); with the final checkers both arms are at ~126/127 effective. The only
consistent `iter4` effect is fewer wasted shell calls, not fewer steps or
tokens. `math_determinant` fails in every run by design: the template's Sarrus
expansion is wrong, still passes its visible identity test, and the model keeps
it while adding checked arithmetic.

First request fell 5535 -> 4394 prompt tokens (-21%) across the iterations.
That prefix is provider-cached, so the cash effect is small; the win is 1.1k
tokens of context headroom and a smaller choice space. Deferring tools did not
add `tool_search` calls in normal work: `exec_command` and `apply_patch` stay at
~99% of calls.

`iter4` vs `iter3` over three runs each: pass 124.7 vs 125.3, steps 4.41 vs
4.46, tokens 24.0k vs 24.1k, cost $0.00180 for both. The mean deltas are inside
the per-task noise (see Variance), but the waste signals moved in the intended
direction over 381 runs per arm: `git status`/`git diff` calls 0.18 -> 0.09 per
task, scratch writes 0.03 -> 0.02, redundant calls 0.07 -> 0.06, `exec_command`
calls 2.43 -> 2.30. So the line helps at the margin and does not hurt
correctness; it does not by itself remove the long tail.

The suite is weighted toward the two categories that drive most real work:
agentic coding and math together hold 91 of 127 tasks, so a regression cannot
hide behind a handful of lucky runs:

| category | tasks |
| --- | --- |
| agentic-coding (40) | the 24 from the 81-task suite plus agentic_dijkstra, agentic_bfs_grid, agentic_union_find, agentic_kadane, agentic_quickselect, agentic_merge_inversions, agentic_min_heap, agentic_trie, agentic_lcs, agentic_knapsack, agentic_coin_change, agentic_nqueens, agentic_next_permutation, agentic_kmp, agentic_spiral, agentic_trap_rain |
| mathematical-reasoning (51) | the 21 from the 81-task suite plus math_sieve, math_totient, math_fibonacci_fast, math_catalan, math_pythagorean, math_bayes, math_expected_value, math_geometric_series, math_continued_fraction, math_mod_linear, math_set_ops, math_induction_cubes, math_simpson, math_newton, math_binomial_prob, math_derangement, math_proof_sqrt2, math_proof_inf_primes, math_proof_induction_odds, math_def_limit, math_def_continuity, math_def_group, math_theorem_rolle, math_proof_cantor, math_totient_sum, math_binomial_mod, math_matrix_pow, math_mod_sqrt, math_convex_hull, math_partition_mod |
| scientific-reasoning (8) | science_kinematics, science_temperature, science_ideal_gas, science_dilution, science_ohms_law, science_molarity, science_energy, science_wave |
| hallucination-control (10) | hallucination_missing_api, hallucination_missing_flag, hallucination_missing_dependency, hallucination_missing_file, hallucination_missing_arg, hallucination_missing_module, hallucination_missing_config, hallucination_missing_trait, hallucination_missing_test, hallucination_fake_std_api |
| instruction-following (10) | instruction_exact, instruction_no_unsafe, instruction_format_output, instruction_api_shape, instruction_answer_format, instruction_no_files, instruction_exact_constant, instruction_range_check, instruction_doc_comment, instruction_no_mut |
| task-planning (8) | planning_pipeline, planning_refactor, planning_dependency, planning_debug, planning_feature, planning_migration, planning_testfirst, planning_multifile |

Every code task is verified twice before it enters the suite: the untouched
template must fail its hidden tests, and a reference solution must pass them.
Check-gated tasks additionally must fail their correctness check on the
template where the check is deterministic (source and path checks).

Math now carries 51 of the 127 tasks because it was the weakest category in the
13-task run. Shallow formula tasks hid the gap: a model can pass them with one
edit and never reason. The added tasks force work that a template cannot fake:

- 8 no-code proof/definition tasks (irrationality of `sqrt(2)`, Euclid's
  infinitude of primes, induction, epsilon-delta limits and continuity, group
  axioms for `Z/5`, Rolle's theorem, Cantor's diagonal argument). They have no
  crate, `editable: []`, and a `files_unchanged` correctness gate; the final
  assistant message must contain the essential steps.
- 6 multi-step algorithms that fail unless the math is right: summatory totient
  at `10^6`, Lucas' theorem at `10^18`, 2x2 matrix powers, Tonelli-Shanks,
  monotone-chain hull area, Euler's pentagonal recurrence.

Proof checks are deliberately tolerant of notation (LaTeX vs Unicode, `+1` vs
"add one") but intolerant of missing steps, so they measure reasoning rather
than formatting. Three checkers were widened after the model produced correct
proofs that used different notation.

`math-full` (all 51 math tasks, new prompt, 1 repeat): **50/51 pass**, mean score
**94.6**, 4.27 steps, 33.2s, 29.4k tokens per task, cache 84.3%. The single
"failure" was `math_proof_induction_odds`, where the model wrote a complete
induction with the variable `n` instead of `k`; the checker only listed `k`
forms. After teaching the matcher to flatten LaTeX/Unicode notation, all eight
proof tasks passed **24/24** across three repeats. So the earlier 75.2 math
score was a shallow-suite artifact, not a reasoning gap; what remains is cost,
not correctness:

| task | steps | time | tokens |
| --- | --- | --- | --- |
| `math_divisors` | 4 | 150s | 25k |
| `math_combinatorics` | 10 | 131s | 84k |
| `math_binomial_mod` | 7 | 116s | 69k |
| `math_partition_mod` | 6 | 101s | 44k |
| `math_binomial_prob` | 8 | 73s | 60k |

These are the tasks that still rewrite their own solution two or three times
(`verification_runs` 2-3). They are the target for the next prompt iteration, not
for more tasks.

### Where the tokens go

Cache reads dominate the token count but not the bill. At list prices for this
model (input 0.15, output 0.6, cache read 0.003 USD per million), one task costs:

| arm | uncached input | cache read | output (reasoning) | USD/task |
| --- | ---: | ---: | ---: | ---: |
| `baseline` | 700.8k | 3.28M | 276.6k | 0.0022 |
| `iter1` | 637.1k | 2.97M | 253.2k | 0.0020 |
| `iter2` | 639.2k | 2.53M | 250.8k | 0.0020 |
| `iter3` | 648.3k | 2.29M | 212.6k | 0.0018 |
| `iter4` | 624.4k | 2.22M | 198.5k | 0.0017 |

Output is 63-65% reasoning tokens (`reasoning_tokens / completion_tokens`), and
cache reads cost 2% of an input token. So the real split is about 37% uncached
input, 37% reasoning, 26% visible output. Deferring tool schemas removed cached
prefix tokens, which is why `iter2` cut total tokens by 20% with almost no cost
change; the later drops come from fewer steps and less reasoning, not schemas.

Per-step measurement (event store kept for six heavy tasks) explains the rest:
the first request is ~4.4k prompt tokens, of which 3584 hit the shared prefix,
so only ~800 are uncached. Every later step adds 800-1500 uncached tokens, largely
one cache block of tail plus new content, independent of tool-output size. Tool
results measured 100-450 tokens; the 4000-token `context.tool_output_tokens` cap
never fired. Lowering it is a no-op on this suite and would only force follow-up
reads on genuinely large output, so it was left alone.

The levers that remain are steps and reasoning tokens, not schemas or output
truncation.

### Variance

Single-run cost numbers are not comparable. Three repeats over the 13 heaviest
tasks (iter3 binary):

- median per-task step SD **1.25**, median token CV **13.9%**;
- `math_isqrt` took 4 steps in one run and 18 in another; `planning_multifile`
  is consistently long (12 ± 1.6 steps, ~99k tokens);
- the same `iter3` binary produced 24.8k, 23.0k and 24.4k tokens/task over
  three full-suite runs, and 125-126/127 pass.

`compare.py` already calls deltas under 2 points noise; for cost axes, use at
least three repeats or compare only medians of the same task set.

### Disk

`/tmp` is a 16G tmpfs and the eval work trees plus cargo targets fill it. Two
full arms were lost to `ENOSPC` before the cause was clear: daemons died with
`daemon closed the connection` after 0-1 steps, `eix new` failed, and some runs
reported `Disk quota exceeded`. Those runs are not in the table. Keep at least
8G free, use three shards rather than four, and delete
`$CARGO_TARGET_DIR/debug/incremental` before a long comparison.

### Checker repairs

Two tasks were self-contradictory and two checkers rejected correct answers:

- `hallucination_missing_file` asked to "fix the retry logic" while gating on
  `files_unchanged`, so a model that found the missing test and still fixed a
  real clamp bug failed. Rewritten as a claim check ("either quote the failing
  test and the slow code, or explain that the test does not exist").
- `hallucination_missing_test` required the reply to repeat the fake test symbol;
  the model correctly named `tests/timeout.rs` instead. The exact-symbol needle
  was removed; `files_unchanged` plus a negative phrase still gate it.
- `hallucination_fake_std_api` and `hallucination_missing_dependency` rejected
  "no `read_text` reference exists" and "nothing in the repo references
  serde_json". The negative-phrase list now also accepts "premise doesn't hold",
  "no reference", "not referenced", "no edit was needed", "no change was
  needed", "premise is incorrect", "nothing to fix" and similar, on all ten
  hallucination tasks.
- `math_proof_inf_primes` required the literal word "contradiction"; a complete
  proof that ends "contradicting the assumption" failed. The needle is now
  `contradict`, which matches both forms.

After the repairs the rewritten tasks pass 3/3 on `iter3` and 2/2 on `baseline`
and `iter2`; the phrasing tasks pass 4/4, the trait/module tasks 3/3, and the
prime proof was already correct.

## What each iteration changed

1. `iter-cwd-cache` - `stream_options.include_usage` so usage is reported on
   every step; workspace path removed from the cached prompt head. First-step
   cache 0% -> ~73%, overall 63.5% -> 77%.
2. `iter-efficiency-rule` - one line: don't add temp tests or delete generated
   files. Steps 7.0 -> 5.2, tokens 305k -> 219k, cache 76.8%.
3. `iter-final-prompt` - shortened rule to stay under the prompt budget.
   Steps 5.0, tokens 209k, cache 76.3%.
4. `iter-workspace-rule` (7-task suite) - add "don't `git diff` your own edit",
   "don't add scratch crates", and "work only inside the workspace; never
   search for hidden tests or graders". Also deletes injected hidden tests after
   each run so the benchmark cannot leak its answer key into `/tmp`.
5. `iter1` (127 tasks) - after a patch, don't re-read, `git diff`/`git status`,
   or rebuild an equivalent verifier; run the workspace tests once, and keep
   work inside the workspace. Steps 4.97 -> 4.55, tokens 33.5k -> 30.4k, pass
   unchanged.
6. `iter2` (127 tasks) - Full mode advertises only tools that can fire:
   `request_user_input` and the four approval-only `exec_command` fields are
   gone, and web/image/LSP built-ins move behind `tool_search`. Sub-agents keep
   the read-only set. First request 5535 -> 4691, no extra tool calls.
7. `iter3` (127 tasks) - defer `spawn_agent`, `memory` and
   `get_context_remaining` too, move the sub-agent guardrail into the
   `spawn_agent` schema, and trim the `exec_command` descriptions. First request
   4691 -> 4371. The saved tokens are cached prefix, so this barely moves
   cost; it buys context headroom and a smaller choice space.
8. `iter4` (127 tasks) - one line after the tests rule: "a green test run ends
   the task: no second run, warning-only rebuilds, `cargo clean`, or git."
   Targets the long tail that keeps touching files and rebuilding after the
   tests already pass. On the same task set: 126/127 vs 125/127, `git` calls
   0.21 -> 0.10 per task, scratch writes 0.06 -> 0.01, `exec_command` 320 -> 289;
   steps and tokens move inside the noise band.

## Open opportunities

- Long-tail verification: `planning_multifile` gets the tests green, then
  `touch`es sources to rebuild, greps warnings, runs `cargo clean`, and takes a
  `git` inventory. The `iter4` line cut git/scratch calls by half and trimmed
  `exec_command` by 10% without losing a task, but the step mean did not move
  outside noise. The next target is the tail itself, not the average.
- Reasoning is ~64% of output and ~37% of cash cost. A stopping-condition line
  or a lower reasoning variant could cut it, but only if the math and proof
  pass rates stay flat; measure correctness before cost.
- A single 127-task pass has ±3 runs of noise: `upstream stream interrupted`
  and daemons dying under four-way build pressure (`daemon closed the
  connection`, 0-1 steps, no panic line in `eixd.log`). Three shards are safer
  than four; the daemon exit path should log a reason instead of closing the
  socket silently.
- `validate_config` shows that an underspecified error payload invites
  probing. Either specify the payload in the task or accept the variance.
- Tool schemas are now ~1.4k tokens in Full mode (down from ~2.4k). The
  remaining fixed overhead is the base prompt and the workspace summary, not
  the tool list; further hiding risks a `tool_search` round for no cash gain.
- Cache misses on slow steps are provider TTL, not prompt churn; only a faster
  loop helps.
