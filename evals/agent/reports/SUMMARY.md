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

`expanded-baseline`: baseline pending.

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

## Open opportunities

- `validate_config` shows that an underspecified error payload invites
  probing. Either specify the payload in the task or accept the variance.
- Tool schemas cost ~2.4k tokens per request (20 schemas). Gating
  capability-specific tools (`web_*`, `view_image`, `lsp_diagnostics`,
  `use_skill`, `spawn_agent`) on whether they can work would cut fixed overhead
  without changing behavior when the capability exists.
- Cache misses on slow steps are provider TTL, not prompt churn; only a faster
  loop helps.
