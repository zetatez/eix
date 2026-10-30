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

Current tasks:

| id | kind | what it exercises |
| --- | --- | --- |
| `fix_pagination` | bug fix | 1-based pagination, empty/out-of-range pages, partial last page |
| `add_slugify` | feature | implement a documented string transform from an empty stub |
| `merge_inventory` | multi-file fix | parser trimming and aggregation across `items.rs` / `report.rs` |

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

- `passed`: hidden tests exit 0 after the agent finishes
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
- The benchmark is intentionally small and synthetic. It is an observation
  tool, not a claim about general coding ability.
- Keep task prompts and hidden tests under version control so results are
  comparable across iterations.
