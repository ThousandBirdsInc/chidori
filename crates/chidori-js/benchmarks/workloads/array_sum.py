# Python twin of array_sum.js — dense-list traversal with index arithmetic:
# reads, in-place writes, a two-list dot product, and a nested 2D walk.
# Deterministic fill (no RNG) so every runtime computes the same checksum.
#
# The checksum is accumulated in float, not int: JS does this math in IEEE
# doubles, and `checksum + s + d` can graze 2^53 where doubles round. Python
# floats reproduce that rounding exactly; exact ints would not.
N = 200_000
ROUNDS = 5
a = [0] * N
b = [0] * N
for i in range(N):
    a[i] = (i * 7919) % 10007
    b[i] = (i * 104729) % 7919
checksum = 0.0
for r in range(ROUNDS):
    # read + accumulate
    s = 0
    for i in range(len(a)):
        s += a[i]
    # dot product
    d = 0
    for i in range(len(a)):
        d += a[i] * b[i]
    # in-place transform
    for i in range(len(a)):
        a[i] = (a[i] + b[i]) % 10007
    checksum = (checksum + s + d) % 9007199254740991
# nested 2D walk
m = 0
for i in range(500):
    for j in range(500):
        m += (i * j) % 13
print("RESULT=" + str(int(checksum + m)))
