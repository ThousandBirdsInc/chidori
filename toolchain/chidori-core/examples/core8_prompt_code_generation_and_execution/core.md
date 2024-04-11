# Demonstrating how a prompt is capable of generating source code and executing it

```prompt (current_weather_in_sf)
---
fn: get_states_first_letters
calling:
  - run_prompt
  - first_letter
---
List the first {{num}} US states to be added to the union.
Return this as a `-` bulleted list with the name of the state on each line.
```


```python (entry)
import unittest

class TestMarshalledValues(unittest.IsolatedAsyncioTestCase):
    async def test_run_prompt(self):
        self.assertEqual(await run_prompt(5), 4)

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
```
