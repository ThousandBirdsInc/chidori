# Python twin of array_hof.js — map/filter/reduce with per-element lambdas.
# The intermediate results are materialized with list() because JS
# Array.prototype.map/filter allocate real arrays, not lazy iterators.
# The final sum stays below 2^53, so exact ints match the JS doubles.
from functools import reduce

N = 200_000
a = []
for i in range(N):
    a.append(i)
result = reduce(
    lambda p, c: p + c,
    list(filter(lambda x: x % 2 == 0, list(map(lambda x: x * x, a)))),
    0,
)
print("RESULT=" + str(result))
