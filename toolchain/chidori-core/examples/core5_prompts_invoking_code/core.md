# Demonstrating how a prompt is capable of being invoked by code and vice versa


```python (run_prompt)
async def run_prompt(number_of_states):
    out = ""
    for state in (await get_states_first_letters(num=number_of_states)).split('\n'):
        out += await first_letter(state)
    return "demo" + out
```


```prompt (states)
---
fn: get_states_first_letters
---
List the first {{num}} US states to be added to the union.
Return this as a `-` bulleted list with the name of the state on each line.
```


```python (first_letter)
def first_letter(s):
    return s[0]
```


```python (entry)
import unittest

class TestMarshalledValues(unittest.IsolatedAsyncioTestCase):
    async def test_run_prompt(self):
        self.assertEqual(await run_prompt(5), 4)

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
```
