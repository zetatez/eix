# Iteration log

Same 3 tasks (`fix_pagination`, `add_slugify`, `merge_inventory`), 2 repeats
each, same model and provider. Quality gate: hidden tests injected after the
agent finishes. A run only counts if all 6 pass.

| run | pass | mean time | steps | tools | tokens | cache hit | usage |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `baseline2` | 6/6 | 21.2s | 5.50 | 4.67 | 230,395 | 63.5% | 100% |
| `iter-cwd-cache` | 6/6 | 32.0s | 7.00 | 6.17 | 305,573 | 77.0% | 100% |
| `iter-efficiency-rule` | 6/6 | 22.4s | 5.17 | 4.33 | 218,916 | 76.8% | 100% |
| `iter-final-prompt` | 6/6 | 22.0s | 5.00 | 4.50 | 209,259 | 76.3% | 100% |

## What each iteration changed

1. `baseline2` - observability baseline. Token accounting was incomplete:
   many model steps had no usage at all.
2. `iter-cwd-cache`
   - `provider.rs`: send `stream_options: {"include_usage": true}` so every
     OpenAI-compatible step reports usage. Usage coverage went 40-60% -> 100%,
     which made the cache numbers trustworthy.
   - `server.rs`: drop `Workspace: {cwd}` from the cached system head. The cwd
     is already in `<environment>`, and embedding it made the cached prefix
     unique per session. First-step cache went 0% -> ~73%.
   - Cost: the extra cache probe runs made this batch noisier in steps/time;
     cache rose 63.5% -> 77.0%.
3. `iter-efficiency-rule`
   - `server.rs`: one line in the working agreement - "Don't add temp tests or
     delete generated files unless asked." The agent had been creating throwaway
     `tests/*.rs` files and deleting `Cargo.lock`, which added 2-4 steps per run.
   - Steps 7.00 -> 5.17, tools 6.17 -> 4.33, tokens 305k -> 219k, cache held at
     76.8%.
4. `iter-final-prompt`
   - `server.rs`: shortened the same rule to keep the base prompt under the
     budget ceiling (840 -> 860 tokens). Steps 5.17 -> 5.00, tokens -> 209k,
     cache 76.3%.

## Reading the numbers

- The pass rate never moved: every iteration kept 6/6. Efficiency was only
  compared within that constraint.
- Cache hit rate is `cache_read_tokens / prompt_tokens`. The first step is
  always a miss by definition; later steps now hit ~76%.
- `iter-cwd-cache` looks slower only because that batch happened to trigger the
  extra verification loops the efficiency rule later removed. Compare it with
  `iter-efficiency-rule`, not in isolation.
- `merge_inventory` is the noisiest task (4-9 steps) because the agent decides
  how much of the multi-file repo to inspect. Treat single-run deltas there as
  noise unless they reproduce.

## Next candidates

- Inject the repo map / failing test summary up front so the agent skips the
  first exploratory `rg --files` + full-file dump.
- Trim the tool schemas (especially `exec_command` docs) to cut fixed prompt
  tokens per request.
- Add a task with a genuinely ambiguous requirement to test whether the
  efficiency rule hurts clarification behavior.
