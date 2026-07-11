# Python twin of closures.js — closure capture + higher-order calls in a loop.
N = 1_000_000


def adder(n):
    def add(x):
        return x + n

    return add


f = adder(5)
s = 0
for i in range(N):
    s = f(s) - 4
print("RESULT=" + str(s))
