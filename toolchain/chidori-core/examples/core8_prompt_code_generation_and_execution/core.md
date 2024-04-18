# Demonstrating how a prompt is capable of generating source code and executing it

```prompt (gen_fib_sequence)
---
eject:
  language: python
  mode: replace
---
Generate a function that returns the Fibonacci sequence up to the nth number. 
The function should be named `fib_sequence` and should accept a single 
argument `n` which is the number of Fibonacci numbers to generate.
```


```python (entry)
import unittest

class TestMarshalledValues(unittest.IsolatedAsyncioTestCase):
    async def test_run_prompt(self):
        self.assertEqual(await run_prompt(5), 4)

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
```
