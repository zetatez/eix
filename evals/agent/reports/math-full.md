# eix agent eval

- model: `deepseek/deepseek-v4.1-flash`
- binary: `/tmp/eix-agent-new-bin`
- tasks: math_angle, math_bayes, math_big_factorial, math_binomial_mod, math_binomial_prob, math_catalan, math_collatz, math_combinatorics, math_complex, math_continued_fraction, math_convex_hull, math_crt, math_def_continuity, math_def_group, math_def_limit, math_derangement, math_determinant, math_divisors, math_expected_value, math_extended_gcd, math_fibonacci_fast, math_gcd_lcm, math_geometric_series, math_induction_cubes, math_isqrt, math_linear, math_matrix, math_matrix_pow, math_mod_linear, math_mod_sqrt, math_modpow, math_newton, math_partition_mod, math_polynomial, math_prime_factors, math_primes, math_proof_cantor, math_proof_induction_odds, math_proof_inf_primes, math_proof_sqrt2, math_pythagorean, math_radix, math_rational, math_set_ops, math_sieve, math_simpson, math_stats, math_theorem_rolle, math_totient, math_totient_sum, math_triangle
- repeats: 1

## Summary

- score: **94.6** / 100
- pass: **50/51** (98%)
- axes: correctness 98, adherence 100, loops 96, tokens 94, time 84, cache 85
- mean duration: **33.2s**
- mean model steps: **4.27**
- mean tool calls: **3.41**
- prompt tokens: **1365873**
- completion tokens: **134232**
- usage coverage: **100%** of model steps
- cache hit rate: **84.3%**

## Runs

| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| math_angle | mathematical-reasoning | yes | 95 A | 25.5s | 5 | 4 | 29995 | 1127 | 86% |
| math_bayes | mathematical-reasoning | yes | 99 A | 18.3s | 4 | 3 | 23778 | 642 | 85% |
| math_big_factorial | mathematical-reasoning | yes | 99 A | 20.8s | 4 | 3 | 23779 | 1129 | 84% |
| math_binomial_mod | mathematical-reasoning | yes | 84 B | 115.8s | 7 | 6 | 52159 | 17050 | 81% |
| math_binomial_prob | mathematical-reasoning | yes | 82 B | 72.9s | 8 | 7 | 52559 | 7697 | 85% |
| math_catalan | mathematical-reasoning | yes | 99 A | 21.9s | 4 | 3 | 23428 | 1702 | 85% |
| math_collatz | mathematical-reasoning | yes | 99 A | 17.5s | 4 | 3 | 23997 | 540 | 84% |
| math_combinatorics | mathematical-reasoning | yes | 74 C | 130.9s | 10 | 9 | 70556 | 13816 | 85% |
| math_complex | mathematical-reasoning | yes | 99 A | 15.8s | 4 | 4 | 24367 | 678 | 84% |
| math_continued_fraction | mathematical-reasoning | yes | 89 B | 92.6s | 5 | 5 | 30171 | 1726 | 87% |
| math_convex_hull | mathematical-reasoning | yes | 95 A | 40.4s | 6 | 5 | 40365 | 4901 | 83% |
| math_crt | mathematical-reasoning | yes | 95 A | 59.0s | 4 | 3 | 24577 | 8452 | 83% |
| math_def_continuity | mathematical-reasoning | yes | 99 A | 5.5s | 1 | 0 | 5491 | 770 | 86% |
| math_def_group | mathematical-reasoning | yes | 99 A | 4.4s | 1 | 0 | 5474 | 614 | 87% |
| math_def_limit | mathematical-reasoning | yes | 99 A | 4.3s | 1 | 0 | 5508 | 424 | 86% |
| math_derangement | mathematical-reasoning | yes | 99 A | 18.0s | 4 | 3 | 23521 | 1078 | 84% |
| math_determinant | mathematical-reasoning | yes | 99 A | 15.0s | 4 | 3 | 26122 | 1234 | 84% |
| math_divisors | mathematical-reasoning | yes | 88 B | 149.9s | 4 | 3 | 24003 | 1818 | 91% |
| math_expected_value | mathematical-reasoning | yes | 86 B | 27.0s | 9 | 9 | 60246 | 1476 | 86% |
| math_extended_gcd | mathematical-reasoning | yes | 99 A | 17.2s | 4 | 3 | 23986 | 1474 | 84% |
| math_fibonacci_fast | mathematical-reasoning | yes | 96 A | 46.7s | 4 | 3 | 23819 | 1929 | 84% |
| math_gcd_lcm | mathematical-reasoning | yes | 98 A | 18.7s | 5 | 4 | 30942 | 1358 | 84% |
| math_geometric_series | mathematical-reasoning | yes | 99 A | 14.3s | 4 | 3 | 23715 | 986 | 85% |
| math_induction_cubes | mathematical-reasoning | yes | 99 A | 14.9s | 4 | 3 | 23383 | 1208 | 85% |
| math_isqrt | mathematical-reasoning | yes | 99 A | 20.1s | 4 | 4 | 24052 | 1839 | 83% |
| math_linear | mathematical-reasoning | yes | 98 A | 17.7s | 5 | 4 | 30533 | 1205 | 84% |
| math_matrix | mathematical-reasoning | yes | 98 A | 25.9s | 4 | 4 | 24692 | 2672 | 83% |
| math_matrix_pow | mathematical-reasoning | yes | 94 A | 32.4s | 7 | 6 | 46017 | 3300 | 84% |
| math_mod_linear | mathematical-reasoning | yes | 98 A | 33.6s | 4 | 3 | 24150 | 4131 | 84% |
| math_mod_sqrt | mathematical-reasoning | yes | 98 A | 34.5s | 4 | 3 | 24759 | 4969 | 83% |
| math_modpow | mathematical-reasoning | yes | 93 A | 70.3s | 4 | 3 | 23639 | 818 | 85% |
| math_newton | mathematical-reasoning | yes | 91 A | 62.1s | 5 | 4 | 31479 | 8890 | 83% |
| math_partition_mod | mathematical-reasoning | yes | 90 B | 100.5s | 6 | 5 | 39487 | 4796 | 84% |
| math_polynomial | mathematical-reasoning | yes | 99 A | 13.5s | 4 | 3 | 23918 | 1077 | 84% |
| math_prime_factors | mathematical-reasoning | yes | 97 A | 30.6s | 4 | 3 | 25788 | 4466 | 82% |
| math_primes | mathematical-reasoning | yes | 99 A | 13.7s | 4 | 3 | 23533 | 578 | 85% |
| math_proof_cantor | mathematical-reasoning | yes | 98 A | 22.2s | 1 | 0 | 5491 | 3500 | 86% |
| math_proof_induction_odds | mathematical-reasoning | no | 40 F | 3.8s | 1 | 0 | 5508 | 546 | 86% |
| math_proof_inf_primes | mathematical-reasoning | yes | 99 A | 5.2s | 1 | 0 | 5496 | 373 | 86% |
| math_proof_sqrt2 | mathematical-reasoning | yes | 99 A | 4.7s | 1 | 0 | 5487 | 468 | 86% |
| math_pythagorean | mathematical-reasoning | yes | 93 A | 39.2s | 7 | 8 | 45741 | 3930 | 84% |
| math_radix | mathematical-reasoning | yes | 99 A | 15.9s | 4 | 3 | 24172 | 988 | 84% |
| math_rational | mathematical-reasoning | yes | 94 A | 44.2s | 6 | 5 | 41010 | 4391 | 83% |
| math_set_ops | mathematical-reasoning | yes | 94 A | 26.7s | 6 | 5 | 39997 | 1867 | 84% |
| math_sieve | mathematical-reasoning | yes | 98 A | 21.5s | 5 | 4 | 30049 | 1397 | 85% |
| math_simpson | mathematical-reasoning | yes | 99 A | 16.3s | 4 | 3 | 23975 | 895 | 84% |
| math_stats | mathematical-reasoning | yes | 99 A | 17.5s | 4 | 3 | 24403 | 759 | 84% |
| math_theorem_rolle | mathematical-reasoning | yes | 98 A | 16.8s | 1 | 0 | 5503 | 366 | 86% |
| math_totient | mathematical-reasoning | yes | 99 A | 11.6s | 4 | 3 | 23430 | 591 | 85% |
| math_totient_sum | mathematical-reasoning | yes | 99 A | 13.7s | 4 | 3 | 23487 | 760 | 84% |
| math_triangle | mathematical-reasoning | yes | 99 A | 12.9s | 4 | 3 | 24136 | 831 | 84% |

## Failures

### math_proof_induction_odds (repeat 1)

- send exit: `0` timed_out=`False`
daemon log tail:
```
eixd 0.0.0 listening on /tmp/eix-agent-eval/20261030T154300Z-math_proof_induction_odds-r1/eix.sock
  model  deepseek/deepseek-v4.1-flash
  api    http://127.0.0.1:3210/v1
  data   /tmp/eix-agent-eval/20261030T154300Z-math_proof_induction_odds-r1/data/eix.sqlite3

```

