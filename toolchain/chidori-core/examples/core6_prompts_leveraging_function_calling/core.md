# Demonstrating how to leverage function calling in prompts

```python (first_letter)
def first_letter(s):
    return s[0]
```

```python (math_demo)
def do_some_math(a, b):
      return a + b
```


```javascript (jsdemo)
function jsMath(a, b) {
    return a + b;
}
```

```typescript (tsdemo)
function tsTypesafe(a: string, b: number) {
    return a + b;
}
```

```prompt (current_weather_in_sf)
---
import:
  - first_letter
  - jsMath
  - do_some_math
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
