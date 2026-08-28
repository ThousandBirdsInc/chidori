# Python twin of checksum.js — Adler-32 over a bytearray (CPython's mutable
# byte buffer, the stdlib analog of Uint8Array).
#
# JS-semantics notes: a and b stay far below 2^31, so JS int32 arithmetic and
# Python ints agree everywhere except the final pack — JS `(b << 16) | a`
# wraps int32 and `>>> 0` reads the bits back unsigned, which for these
# ranges equals the exact b * 65536 + a; Python computes that directly.
def main():
    N = 500_000
    ROUNDS = 10
    MOD = 65521
    data = bytearray(N)
    for i in range(N):
        data[i] = (i * 131 + 7) & 0xFF
    result = 0
    for r in range(ROUNDS):
        a = 1
        b = 0
        for i in range(len(data)):
            a = (a + data[i]) % MOD
            b = (b + a) % MOD
        adler = (b << 16) | a
        result = (result + adler) % 9007199254740991
        # Perturb one byte so rounds are not identical.
        data[r] = (data[r] + 1) & 0xFF
    print("RESULT=" + str(result))


main()
