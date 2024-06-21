# This should demonstrate a variety of function calling interactions between different cell types and configurations

## Simple function calling across cells
### Demonstrates defining a function in python and calling it in javascript
```python (python_add_two)
def add_two(x):
    return x + 2
```

```javascript
import { assertEquals } from "https://deno.land/std@0.221.0/assert/mod.ts";

Deno.test("addition test", async () => {
    const result = await add_two(2);
    console.log(result);
    assertEquals(result, 4);
});
```

### Demonstrates defining a function in javascript and calling it in python
```javascript (js_add_two)
function addTwo(x) {
    return x + 2;
}
```

```python
import unittest

class TestMarshalledValues(unittest.TestCase):
    async def test_addTwo(self):
        self.assertEqual(await addTwo(2), 4)

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
```
