# Python twin of mutual_recursion.js — mutual recursion between two globals,
# boolean returns, and self-recursion through a local binding (gcd). Max
# depth is ~300 (is_even(299)), comfortably inside CPython's default 1000
# recursion limit.
def is_even(n):
    return True if n == 0 else is_odd(n - 1)


def is_odd(n):
    return False if n == 0 else is_even(n - 1)


gcd = lambda a, b: a if b == 0 else gcd(b, a % b)  # noqa: E731 — mirrors the JS arrow

N = 20_000
checksum = 0
for i in range(N):
    if is_even(i % 300):
        checksum += 1
    checksum += gcd(i + 123456, 991)
print("RESULT=" + str(checksum))
