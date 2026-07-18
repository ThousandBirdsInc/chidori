# Python twin of arith_loop.js — tight numeric loop (interpreter dispatch +
# arithmetic). All values stay far below 2^53, so exact Python ints match the
# exact JS doubles digit for digit.
N = 1_000_000
s = 0
for i in range(N):
    s += i * 2 - (i % 3)
print("RESULT=" + str(s))
