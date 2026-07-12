# Python twin of array_push_sum.js — list growth + indexed read loop.
# Indexed `for i in range(len(a))` (not `for x in a`) to mirror the JS loop.
N = 500_000
a = []
for i in range(N):
    a.append(i)
s = 0
for i in range(len(a)):
    s += a[i]
print("RESULT=" + str(s))
