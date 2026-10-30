# eix agent eval

A small, deterministic benchmark for measuring how well the eix agent finishes
real coding tasks, and how efficiently it does so.

The harness drives the same binaries users run. It creates a throwaway git
repository per task, starts an isolated `eixd`, gives the agent full mode, then
applies hidden tests and records quality and cost metrics from the event log.

## Task layout

```
tasks/<id>/
  task.json        prompt + verify command
  template/        the repo the agent starts from (already has failing tests)
  hidden/          tests copied in only after the agent finishes
```

The hidden tests are the quality gate. The visible tests in `template/` tell the
agent what is broken without revealing every edge case.

Current tasks (127, grouped by category):

| id | category | what it exercises |
| --- | --- | --- |
| `fix_pagination` | agentic-coding | 1-based pagination, empty/out-of-range pages, partial last page |
| `add_slugify` | agentic-coding | implement a documented string transform from an empty stub |
| `merge_inventory` | agentic-coding | parser trimming and aggregation across `items.rs` / `report.rs` |
| `refactor_dupes` | agentic-coding | factor out duplicated subtotal, fix overflow and underflow |
| `validate_config` | agentic-coding | typed errors, duplicate keys, trimming across two modules |
| `counter_saturate` | agentic-coding | replace panicking arithmetic with saturating arithmetic |
| `state_machine` | agentic-coding | complete a transition table, keep terminal states terminal |
| `agentic_wordwrap` | agentic-coding | greedy word wrap, whitespace normalisation, long words |
| `agentic_lru` | agentic-coding | LRU eviction order, updates, zero capacity |
| `agentic_csv` | agentic-coding | quoted fields, escaped quotes, line parsing |
| `agentic_intervals` | agentic-coding | sort and merge overlapping or touching intervals |
| `agentic_ring_buffer` | agentic-coding | evict the oldest entry, preserve oldest-to-newest order, zero capacity |
| `agentic_json_escape` | agentic-coding | JSON escapes, control characters, invalid escape rejection |
| `agentic_search_bounds` | agentic-coding | lower/upper bound with duplicates and missing values |
| `agentic_roman` | agentic-coding | canonical Roman numerals both ways, reject non-canonical input |
| `agentic_semver` | agentic-coding | strict three-part version parsing and numeric ordering |
| `agentic_toposort` | agentic-coding | deterministic topological order, cycle and bad-edge detection |
| `agentic_rle` | agentic-coding | run-length encode/decode, multi-digit counts, malformed input |
| `agentic_cli_args` | agentic-coding | flag parsing with exact error messages and last-value-wins |
| `agentic_token_bucket` | agentic-coding | refill, cap, saturating ticks, zero rate |
| `agentic_glob` | agentic-coding | `*` and `?` full-string matching over Unicode characters |
| `agentic_utf8_truncate` | agentic-coding | byte budget without splitting characters or panicking |
| `agentic_edit_distance` | agentic-coding | Levenshtein distance over characters |
| `agentic_ledger` | agentic-coding | fix parsing, overdraft checks and accounting across three modules |
| `agentic_dijkstra` | agentic-coding | weighted shortest paths, unreachable nodes, duplicate edges |
| `agentic_bfs_grid` | agentic-coding | grid BFS around walls, bounds checks, unreachable goals |
| `agentic_union_find` | agentic-coding | disjoint sets with path compression, component counts |
| `agentic_kadane` | agentic-coding | maximum subarray sum with indices, all-negative input |
| `agentic_quickselect` | agentic-coding | average-linear k-th order statistic without sorting |
| `agentic_merge_inversions` | agentic-coding | stable merge sort plus inversion counting |
| `agentic_min_heap` | agentic-coding | binary min-heap push/pop with duplicates |
| `agentic_trie` | agentic-coding | word and prefix lookup, empty-string edge cases |
| `agentic_lcs` | agentic-coding | longest common subsequence over Unicode characters |
| `agentic_knapsack` | agentic-coding | 0/1 knapsack dynamic programming, greedy traps |
| `agentic_coin_change` | agentic-coding | minimum coin count, impossible amounts, non-greedy optima |
| `agentic_nqueens` | agentic-coding | n-queens backtracking counts up to 9x9 |
| `agentic_next_permutation` | agentic-coding | next lexicographic permutation with duplicates and wrap |
| `agentic_kmp` | agentic-coding | overlapping pattern matches, Unicode byte offsets |
| `agentic_spiral` | agentic-coding | clockwise spiral traversal of rectangular matrices |
| `agentic_trap_rain` | agentic-coding | trapped rainwater with prefix/suffix maxima |
| `math_rational` | mathematical-reasoning | exact reduced fractions, overflow-safe add/mul |
| `math_combinatorics` | mathematical-reasoning | exact factorial/binomial without intermediate overflow |
| `math_gcd_lcm` | mathematical-reasoning | signed gcd, zero cases, `u64` overflow in lcm |
| `math_stats` | mathematical-reasoning | mean/variance/median, empty and non-finite input |
| `math_primes` | mathematical-reasoning | primality edge cases and a prime list |
| `math_linear` | mathematical-reasoning | 2x2 systems, singular and non-finite inputs |
| `math_modpow` | mathematical-reasoning | modular exponentiation, large exponents, zero modulus |
| `math_matrix` | mathematical-reasoning | transpose/multiply, shape checks, `i64` overflow |
| `math_radix` | mathematical-reasoning | base 2-36 conversion, invalid digits, `u64` overflow |
| `math_polynomial` | mathematical-reasoning | Horner evaluation with checked overflow, derivative |
| `math_complex` | mathematical-reasoning | complex add/multiply/divide and magnitude |
| `math_extended_gcd` | mathematical-reasoning | Bézout coefficients for positive, negative and zero inputs |
| `math_prime_factors` | mathematical-reasoning | grouped prime factorization with exponents |
| `math_determinant` | mathematical-reasoning | 3x3 determinant with checked `i64` overflow |
| `math_isqrt` | mathematical-reasoning | integer square root over the full `u64` range |
| `math_triangle` | mathematical-reasoning | strict triangle inequality, classification, Heron area |
| `math_angle` | mathematical-reasoning | normalise to `[0, 360)` and unit conversions |
| `math_crt` | mathematical-reasoning | Chinese remainder theorem with coprimality and overflow checks |
| `math_big_factorial` | mathematical-reasoning | exact big-integer factorials without external crates |
| `math_divisors` | mathematical-reasoning | divisor lists, proper sums, perfect numbers |
| `math_collatz` | mathematical-reasoning | step counts, maxima, `u64` overflow detection |
| `math_sieve` | mathematical-reasoning | sieve of Eratosthenes prime enumeration |
| `math_totient` | mathematical-reasoning | Euler's totient via prime factorization |
| `math_fibonacci_fast` | mathematical-reasoning | fast-doubling Fibonacci modulo for `n = 10^9` |
| `math_catalan` | mathematical-reasoning | Catalan numbers with `u64` overflow detection |
| `math_pythagorean` | mathematical-reasoning | primitive triples from Euclid's formula |
| `math_bayes` | mathematical-reasoning | Bayes' theorem with probability validation |
| `math_expected_value` | mathematical-reasoning | discrete mean and variance, distribution validation |
| `math_geometric_series` | mathematical-reasoning | finite geometric sums including ratio 1 |
| `math_continued_fraction` | mathematical-reasoning | finite continued-fraction evaluation, zero denominators |
| `math_mod_linear` | mathematical-reasoning | linear congruences via extended Euclid |
| `math_set_ops` | mathematical-reasoning | union/intersection/difference as sorted sets |
| `math_induction_cubes` | mathematical-reasoning | sum-of-cubes closed form with overflow checks |
| `math_simpson` | mathematical-reasoning | composite Simpson integration, even-subinterval rule |
| `math_newton` | mathematical-reasoning | Newton's method square root, convergence limits |
| `math_binomial_prob` | mathematical-reasoning | binomial probability mass with edge cases |
| `math_derangement` | mathematical-reasoning | derangement recurrence with `u64` overflow |
| `math_proof_sqrt2` | mathematical-reasoning | contradiction proof that `sqrt(2)` is irrational |
| `math_proof_inf_primes` | mathematical-reasoning | Euclid's proof that there are infinitely many primes |
| `math_proof_induction_odds` | mathematical-reasoning | induction proof of the sum of the first `n` odd numbers |
| `math_def_limit` | mathematical-reasoning | epsilon-delta limit definition applied to a linear function |
| `math_def_continuity` | mathematical-reasoning | epsilon-delta continuity of `x^2` at a point |
| `math_def_group` | mathematical-reasoning | group axioms verified for `Z/5` under addition |
| `math_theorem_rolle` | mathematical-reasoning | Rolle's theorem hypotheses and the guaranteed point |
| `math_proof_cantor` | mathematical-reasoning | Cantor's diagonal argument for uncountability |
| `math_totient_sum` | mathematical-reasoning | summatory totient via a sieve up to one million |
| `math_binomial_mod` | mathematical-reasoning | Lucas' theorem for `C(n, k) mod p` at `10^18` |
| `math_matrix_pow` | mathematical-reasoning | 2x2 binary matrix exponentiation with huge exponents |
| `math_mod_sqrt` | mathematical-reasoning | Tonelli-Shanks modular square roots |
| `math_convex_hull` | mathematical-reasoning | monotone-chain convex hull and doubled area |
| `math_partition_mod` | mathematical-reasoning | Euler pentagonal recurrence for partition counts |
| `science_kinematics` | scientific-reasoning | projectile equations plus input validation |
| `science_temperature` | scientific-reasoning | Celsius/Fahrenheit/Kelvin and absolute zero |
| `science_ideal_gas` | scientific-reasoning | `PV = nRT` solved three ways, input validation |
| `science_dilution` | scientific-reasoning | `C1V1 = C2V2` solved two ways |
| `science_ohms_law` | scientific-reasoning | `V = IR`, `P = VI`, resistance validation |
| `science_molarity` | scientific-reasoning | mass/moles/molarity conversions, positive inputs |
| `science_energy` | scientific-reasoning | kinetic and gravitational potential energy |
| `science_wave` | scientific-reasoning | speed/frequency/wavelength relations |
| `hallucination_missing_api` | hallucination-control | report a missing function instead of inventing it |
| `hallucination_missing_flag` | hallucination-control | report a missing CLI flag |
| `hallucination_missing_dependency` | hallucination-control | report a missing Cargo dependency |
| `hallucination_missing_file` | hallucination-control | report a missing integration test file |
| `hallucination_missing_arg` | hallucination-control | report a parameter the prompt invents |
| `hallucination_missing_module` | hallucination-control | report a `src/net.rs` module that does not exist |
| `hallucination_missing_config` | hallucination-control | report a `config.toml` key that does not exist |
| `hallucination_missing_trait` | hallucination-control | report a `Draw::render` impl that does not exist |
| `hallucination_missing_test` | hallucination-control | report a test file/symbol that does not exist |
| `hallucination_fake_std_api` | hallucination-control | reject invented `std::fs::read_text`, keep valid code |
| `instruction_exact` | instruction-following | exact API, banned constructs, edit scope |
| `instruction_no_unsafe` | instruction-following | banned `unsafe`/`unwrap`/`expect`/`panic!` |
| `instruction_format_output` | instruction-following | exact `H:MM:SS` format, no debug output |
| `instruction_api_shape` | instruction-following | add one function, leave Cargo.toml and existing API alone |
| `instruction_answer_format` | instruction-following | exact three-line reply, no file changes |
| `instruction_no_files` | instruction-following | explain a risk without touching the workspace |
| `instruction_exact_constant` | instruction-following | add one exact constant, change nothing else |
| `instruction_range_check` | instruction-following | add one validation branch with the given error variant |
| `instruction_doc_comment` | instruction-following | exact doc comment, body and manifest untouched |
| `instruction_no_mut` | instruction-following | remove `let mut` while keeping the result correct |
| `planning_pipeline` | task-planning | call `update_plan` and compose three modules |
| `planning_refactor` | task-planning | plan, extract a shared helper, keep the API |
| `planning_dependency` | task-planning | plan, parse then summarize in dependency order |
| `planning_debug` | task-planning | plan and fix one bug in each of two modules |
| `planning_feature` | task-planning | plan and add two related helpers |
| `planning_migration` | task-planning | plan a rename across three modules |
| `planning_testfirst` | task-planning | plan around failing tests, then fix the transform |
| `planning_multifile` | task-planning | plan lexer/parser work across two new modules |

Each `task.json` also declares:

```json
"category": "agentic-coding",
"editable": ["src/lib.rs"],
"budgets": {"steps": [4, 10], "tokens": [28000, 90000], "seconds": [18, 90]},
"checks": [{"kind": "source_forbidden", "axis": "adherence", "path": "src/lib.rs", "patterns": ["\\.unwrap\\("]}],
"planning": false
```

`editable` is the set of files the task expects to change; touching anything
else costs adherence points. `budgets` are `[target, worst]` used by the scoring
axes. `category` groups the summary. `planning: true` turns on the planning axis.
`checks` are structural assertions run on the agent's own output:

| kind | meaning |
| --- | --- |
| `files_unchanged` | no files were modified (hallucination tasks) |
| `path_unchanged` | a specific path was not modified (e.g. `Cargo.toml`) |
| `text_all` | the final message contains every needle |
| `text_any_of` | the final message contains at least one needle |

`text_all` / `text_any_of` match case-insensitively and tolerate notation:
LaTeX wrappers (`x^{2}`, `\(n\)-th`, `\frac{\varepsilon}{3}`), Unicode
superscripts (`x²`) and the Unicode minus are flattened before comparing, so a
correct proof is not failed for its typesetting. Proof tasks still require the
essential steps, so leniency applies to notation only.
| `source_forbidden` | the source file has none of the patterns (test modules ignored by default) |
| `source_contains` | the source file contains every needle |
| `plan_used` | `update_plan` was called with enough steps, most completed |

Checks with `axis: correctness` gate the pass alongside the hidden tests; other
axes only feed the score, so a stylistic instruction cannot hide a broken
implementation.

## Run

```sh
# build once from the worktree under test
cargo build

# all tasks, one repeat
python3 evals/agent/run_eval.py

# one task, custom model/output
python3 evals/agent/run_eval.py --tasks fix_pagination --repeats 2
```

Useful flags: `--bin-dir`, `--config`, `--timeout`, `--workdir`, `--output`,
`--repeats`.

The harness uses the provider and credentials from the config (by default
`~/.config/eix/config.toml`). It never reads or prints the API key. Each run
gets its own `EIX_DATA_DIR` and Unix socket under `/tmp/eix-agent-eval`.

Outputs:

- `<output>.json`: full records, including per-model-step usage and daemon logs
- `<output>.md`: concise table for comparing iterations

See [`reports/SUMMARY.md`](reports/SUMMARY.md) for the current iteration log and
the baseline-vs-final comparison.

## Metrics

Quality:

- `passed`: hidden tests exit 0 and every `correctness` check passes
- `checks`: per-check `{kind, axis, ok, detail}` records
- `changes`: files touched by the agent (from `git status`)

Speed:

- `duration_s`: wall time of `eix send`, including every model/tool round
- `model_steps`: assistant messages, i.e. requests sent to the model
- `tool_calls`: tool calls the agent made
- `verify_duration_s`: verification time, reported separately from agent time

Cost:

- `prompt_tokens` / `completion_tokens`
- `cache_read_tokens`: prompt tokens served from the provider prefix cache
- `uncached_prompt_tokens`: prompt tokens billed without a cache hit
- `cache_hit_rate`: `cache_read_tokens / prompt_tokens`
- per-step usage in `metrics.steps`, so cache misses can be traced to the step
  where the prefix changed

The primary score is `pass_rate`. Efficiency is only compared between runs with
the same pass rate; a cheaper run that fails a hidden test is a regression.

Process signals (from the tool-call stream):

- `redundant_tool_calls`: an identical `(tool, arguments)` pair issued twice
- `scratch_writes`: temp files written, test files added, generated files deleted
- `verification_runs`: `cargo test` / `cargo check` invocations
- `git_status_calls`: `git status` invocations
- `reads_before_first_edit`: read-only calls before the first `apply_patch`
- `first_step_cache_hit`: cache hit rate of the first model step
- `plan_updates` / `plan_steps` / `plan_completed`: from the last `update_plan`
  call

## Score

One number is easy to game and hides the weak axis, so a run is graded on seven
axes (`evals/agent/score.py`):

| axis | weight | what it rewards |
| --- | ---: | --- |
| correctness | 35 | hidden tests and correctness checks pass |
| adherence | 15 | diff stays inside `editable`, no scratch writes, constraints hold |
| planning | 8 | `update_plan` used and steps completed (planning tasks only) |
| loops | 12 | `model_steps` against the task budget |
| tokens | 12 | prompt + completion tokens against the task budget |
| time | 10 | wall seconds against the task budget |
| cache | 8 | overall prefix-cache reuse and first-step reuse |

Each budget component maps linearly from `target` (1.0) to `worst` (0.0) and is
clamped. A failed run is capped at 40/100 regardless of its other axes: a wrong
answer is not "efficient". The grade is A >= 90, B >= 80, C >= 70, D >= 60.
`planning` is dropped and the weights renormalised on non-planning tasks and on
old reports that predate the axis.

Re-score any report without re-running:

```sh
python3 evals/agent/score.py evals/agent/reports/baseline.json
```

Compare two runs task by task, with regressions and noise called out:

```sh
python3 evals/agent/compare.py reports/before.json reports/after.json
```

`compare.py` exits non-zero when a task loses a passing repeat, so it can gate
an iteration. Score deltas below `--min-delta` (default 2 points) are labelled
`noise` instead of being treated as progress.

The report JSON stores the score per run (`score.total`, `score.components`,
`score.detail`) and the summary stores axis means, so a regression can be
localised instead of guessed.

## Iteration protocol

1. Record a baseline at the current commit (all tasks, at least one repeat).
2. Pick one bottleneck: failures first, then model steps, tokens, or cache hit
   rate. Do not trade quality for cost.
3. Make the smallest change that addresses it, with unit/integration coverage.
4. Re-run `cargo test --workspace` and the eval with the same tasks/model.
5. Keep the change only if pass rate does not drop and the targeted metric
   improves. Otherwise revert it.
6. Repeat from step 2.

Variation between model runs exists. Treat a one-task difference as noise until
it reproduces with `--repeats 2` or more.

## Notes

- Tasks run in `full` mode so `exec_command` and workspace edits do not block on
  approvals. Hard-deny rules still apply.
- Hidden tests are copied in only after the agent stops, and are deleted again
  in the harness `finally` block. Leaving them on disk would let a later run (or
  a filesystem search) read the answer key.
- The agent is told to work only inside the workspace and not to search for
  hidden tests or graders. If a run still spends steps probing outside the
  repo, that shows up as extra steps and `scratch_writes` in the score.
- Provider prefix caches have a TTL. A slow step (cold `cargo test`) can outlive
  it and show a 0% hit on a later step; that is a provider effect, not a prompt
  regression. Compare cache axis means over several repeats.
- The benchmark is intentionally small and synthetic. It is an observation
  tool, not a claim about general coding ability.
- Keep task prompts and hidden tests under version control so results are
  comparable across iterations.
