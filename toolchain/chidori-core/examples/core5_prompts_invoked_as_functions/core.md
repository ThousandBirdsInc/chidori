# Demonstrating how a prompt is capable of being invoked by code

This is a python function that is invoking a prompt by name, kwargs to this
invocation are passed to the prompt. Prompts are async and return strings.
```python (run_prompt)
def first_letter(s):
    return s.replace("-", "").strip()[0]

async def run_prompt(number_of_states):
    out = ""
    for state in (await get_states_first_letters(num=number_of_states)).split('\n'):
        out += first_letter(state)
    return "demo" + out
```

This is the prompt itself. The cell name is used to refer to the prompt output when it is satisfied
by globally available values. The fn key is used to name the prompt in the context of a function invocation.
```prompt (states)
---
model: gpt-3.5-turbo
fn: get_states_first_letters
---
List the first {{num}} US states to be added to the union.
Return this as a `-` bulleted list with the name of the state on each line.
```

A unit test demonstrates the invocation of the prompt by the function.
```python (entry)
import unittest

class TestMarshalledValues(unittest.IsolatedAsyncioTestCase):
    async def test_run_prompt(self):
        self.assertEqual(await run_prompt(5), "demoDPNGC")

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
```
