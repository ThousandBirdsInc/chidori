# Python twin of fib_recursive.js — recursion + function-call overhead
# (frame setup/teardown).
def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)


print("RESULT=" + str(fib(30)))
