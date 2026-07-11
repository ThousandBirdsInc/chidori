# Python twin of typed_array.js — numeric-buffer traffic over array('d')
# (CPython's unboxed float64 buffer, the stdlib analog of Float64Array):
# fill, sum, dot product, in-place transform, plus an int32 bit-mix pass.
#
# JS-semantics notes: the float64 work is IEEE doubles on both sides, so the
# checksum matches by construction (Python float % and JS % agree for
# positive operands). Int32Array wrap-around is reproduced with an explicit
# to_int32 — array('i') would raise OverflowError instead of wrapping, so the
# mix pass runs on plain ints kept in int32 range.
from array import array

N = 50_000
ROUNDS = 4
a = array("d", bytes(8 * N))
b = array("d", bytes(8 * N))
for i in range(N):
    a[i] = (i * 7919) % 10007
    b[i] = (i * 104729) % 7919
checksum = 0.0
for r in range(ROUNDS):
    s = 0.0
    for i in range(len(a)):
        s += a[i]
    d = 0.0
    for i in range(len(a)):
        d += a[i] * b[i]
    for i in range(len(a)):
        a[i] = (a[i] + b[i]) % 10007
    checksum = (checksum + s + d) % 9007199254740991


# Int32 pass: integer wrap + shift semantics (JS `| 0`).
def to_int32(x):
    return ((x & 0xFFFFFFFF) ^ 0x80000000) - 0x80000000


m = [0] * 1024
for i in range(len(m)):
    m[i] = to_int32(i * 2654435761)
mix = 0
for r in range(50):
    for i in range(len(m)):
        mix = to_int32((mix ^ m[i]) + to_int32(mix << 5))
print("RESULT=" + str(int(checksum + (mix % 4294967296))))
