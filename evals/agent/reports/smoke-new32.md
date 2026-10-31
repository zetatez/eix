# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: agentic_bfs_grid, agentic_coin_change, agentic_dijkstra, agentic_kadane, agentic_kmp, agentic_knapsack, agentic_lcs, agentic_merge_inversions, agentic_min_heap, agentic_next_permutation, agentic_nqueens, agentic_quickselect, agentic_spiral, agentic_trap_rain, agentic_trie, agentic_union_find, math_bayes, math_binomial_prob, math_catalan, math_continued_fraction, math_derangement, math_expected_value, math_fibonacci_fast, math_geometric_series, math_induction_cubes, math_mod_linear, math_newton, math_pythagorean, math_set_ops, math_sieve, math_simpson, math_totient
- repeats: 1

## Summary

- score: **95.8** / 100
- pass: **32/32** (100%)
- axes: correctness 100, adherence 99, loops 95, tokens 93, time 88, cache 85
- mean duration: **29.6s**
- mean model steps: **4.75**
- mean tool calls: **3.78**
- prompt tokens: **1018158**
- completion tokens: **76951**
- usage coverage: **100%** of model steps
- cache hit rate: **85.2%**

## Runs

| task                     | category               | pass | score | time  | steps | tools | prompt | completion | cache hit |
| ---                      | ---                    | ---  | ---   | ---:  | ---:  | ---:  | ---:   | ---:       | ---:      |
| agentic_dijkstra         | agentic-coding         | yes  | 99 A  | 21.5s | 4     | 3     | 26111  | 979        | 84%       |
| agentic_bfs_grid         | agentic-coding         | yes  | 99 A  | 15.1s | 4     | 3     | 26026  | 960        | 85%       |
| agentic_union_find       | agentic-coding         | yes  | 99 A  | 15.8s | 4     | 3     | 25951  | 1194       | 84%       |
| agentic_kadane           | agentic-coding         | yes  | 97 A  | 34.9s | 4     | 3     | 25467  | 4153       | 85%       |
| agentic_quickselect      | agentic-coding         | yes  | 99 A  | 22.0s | 4     | 3     | 25575  | 2318       | 85%       |
| agentic_merge_inversions | agentic-coding         | yes  | 99 A  | 20.1s | 5     | 4     | 32260  | 940        | 86%       |
| agentic_min_heap         | agentic-coding         | yes  | 99 A  | 11.6s | 4     | 3     | 25602  | 719        | 85%       |
| agentic_trie             | agentic-coding         | yes  | 99 A  | 15.6s | 4     | 4     | 26258  | 1157       | 84%       |
| agentic_lcs              | agentic-coding         | yes  | 99 A  | 17.7s | 4     | 3     | 25403  | 1002       | 85%       |
| agentic_knapsack         | agentic-coding         | yes  | 84 B  | 61.3s | 7     | 6     | 56114  | 8701       | 84%       |
| agentic_coin_change      | agentic-coding         | yes  | 98 A  | 19.8s | 5     | 4     | 32456  | 1466       | 86%       |
| agentic_nqueens          | agentic-coding         | yes  | 90 A  | 44.3s | 8     | 7     | 54531  | 4436       | 86%       |
| agentic_next_permutation | agentic-coding         | yes  | 99 A  | 11.4s | 4     | 3     | 25341  | 752        | 85%       |
| agentic_kmp              | agentic-coding         | yes  | 99 A  | 12.2s | 4     | 3     | 25041  | 730        | 85%       |
| agentic_spiral           | agentic-coding         | yes  | 95 A  | 50.5s | 4     | 3     | 25720  | 1504       | 85%       |
| agentic_trap_rain        | agentic-coding         | yes  | 98 A  | 25.6s | 5     | 4     | 32223  | 1468       | 85%       |
| math_sieve               | mathematical-reasoning | yes  | 99 A  | 20.6s | 4     | 3     | 25141  | 1299       | 86%       |
| math_totient             | mathematical-reasoning | yes  | 95 A  | 45.6s | 5     | 4     | 31699  | 1135       | 88%       |
| math_fibonacci_fast      | mathematical-reasoning | yes  | 98 A  | 31.9s | 4     | 3     | 25609  | 2632       | 85%       |
| math_catalan             | mathematical-reasoning | yes  | 98 A  | 23.7s | 4     | 3     | 25038  | 2289       | 85%       |
| math_pythagorean         | mathematical-reasoning | yes  | 70 D  | 70.1s | 13    | 12    | 104924 | 6010       | 87%       |
| math_bayes               | mathematical-reasoning | yes  | 99 A  | 16.6s | 4     | 3     | 25400  | 650        | 85%       |
| math_expected_value      | mathematical-reasoning | yes  | 99 A  | 17.8s | 4     | 3     | 25714  | 793        | 85%       |
| math_geometric_series    | mathematical-reasoning | yes  | 90 B  | 32.0s | 6     | 5     | 39613  | 2868       | 86%       |
| math_continued_fraction  | mathematical-reasoning | yes  | 98 A  | 24.6s | 4     | 3     | 25435  | 2112       | 85%       |
| math_mod_linear          | mathematical-reasoning | yes  | 89 B  | 74.1s | 6     | 5     | 45814  | 9956       | 83%       |
| math_set_ops             | mathematical-reasoning | yes  | 95 A  | 44.7s | 4     | 3     | 25854  | 857        | 85%       |
| math_induction_cubes     | mathematical-reasoning | yes  | 98 A  | 22.5s | 4     | 3     | 24986  | 1446       | 86%       |
| math_simpson             | mathematical-reasoning | yes  | 99 A  | 17.1s | 4     | 3     | 25682  | 653        | 85%       |
| math_newton              | mathematical-reasoning | yes  | 95 A  | 43.9s | 4     | 3     | 25334  | 5169       | 85%       |
| math_binomial_prob       | mathematical-reasoning | yes  | 95 A  | 46.6s | 4     | 3     | 26715  | 5943       | 83%       |
| math_derangement         | mathematical-reasoning | yes  | 99 A  | 16.2s | 4     | 3     | 25121  | 660        | 85%       |

## Failures

None.
