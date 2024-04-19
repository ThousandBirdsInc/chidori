# Demonstrating how to leverage function calling in prompts

```python (math_fn)
def add_two_numbers(a, b):
      return a + b
```


```prompt (add_population)
---
fn: add_population
model: gpt-3.5-turbo
import:
  - add_two_numbers
---
Add the population of {{state}} to the population of California
```


```python (entry)
import unittest

class TestMarshalledValues(unittest.IsolatedAsyncioTestCase):
    async def test_run_prompt(self):
        self.assertEqual(await add_population(state="Arizona"), 4)

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
```
