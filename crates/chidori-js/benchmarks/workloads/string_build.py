# Python twin of string_build.js — string building via += in a loop
# (concatenation + number→string coercion). CPython's in-place str realloc
# optimization makes this its natural fast path, just as ropes would for a JS
# engine — both are fair game, it's the same idiom.
N = 30_000
s = ""
for i in range(N):
    s += "x" + str(i)
print("RESULT=" + str(len(s)))
