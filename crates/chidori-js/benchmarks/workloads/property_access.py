# Python twin of property_access.js — object attribute get/set in a loop.
# A plain (dict-backed) instance is CPython's equivalent of a JS object with
# named properties.
N = 1_000_000


class Obj:
    pass


o = Obj()
o.a = 0
o.b = 0
o.c = 0
for i in range(N):
    o.a = i
    o.b = o.a + 1
    o.c = o.b + o.a
print("RESULT=" + str(o.c))
