# Demonstrating how a prompt is capable of generating source code and immediately executing it

```codegen (gen_fib_sequence)
---
model: gpt-3.5-turbo
language: python
---
Generate a function that returns the Fibonacci sequence up to the nth number. 
The function should be named `fib_sequence` and should accept a single 
argument `n` which is the amount of the Fibonacci sequence to generate.
```


```python (entry)
out = gen_fib_sequence(10)
```
