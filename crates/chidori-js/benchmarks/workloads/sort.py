# Python twin of sort.js — sorting with a deterministic LCG fill.
#
# Two deliberate divergences from a naive line-by-line port, both needed to
# keep RESULT identical to the JS runtimes:
#
# - The JS LCG multiplies a 32-bit seed by 1103515245 as a *double*, which
#   overflows 2^53 and rounds before `>>> 0` truncates to uint32. Python ints
#   would compute that product exactly and drift from every JS engine, so the
#   multiply-add is done in float (IEEE double, same rounding) and int() % 2^32
#   reproduces ToUint32.
# - list.sort() takes no comparator; Python's natural int ordering is what the
#   JS `(x, y) => x - y` comparator requests. The comparator-call overhead the
#   JS side measures has no idiomatic CPython equivalent (cmp_to_key would
#   measure its wrapper, not the sort).
N = 50_000
ROUNDS = 6
seed = 123456789


def rnd():
    global seed
    seed = int(seed * 1103515245.0 + 12345.0) % 4294967296
    return seed


checksum = 0
for r in range(ROUNDS):
    a = [0] * N
    for i in range(N):
        a[i] = rnd()
    a.sort()
    checksum = (checksum + a[0] + a[N - 1] + a[N >> 1]) % 4294967296
print("RESULT=" + str(checksum))
