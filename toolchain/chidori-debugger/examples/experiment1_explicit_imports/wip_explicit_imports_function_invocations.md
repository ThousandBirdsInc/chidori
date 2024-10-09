# This should demonstrate a variety of function calling interactions between different cell types and configurations

## Simple function calling across cells
### Demonstrates defining a function in python and calling it in javascript
```python
def add_two(x):
    return x + 2
```

```javascript
const { add_two } = require('./python_module');

test('add_two function', () => {
    expect(add_two(2)).toBe(4);
});
```

### Demonstrates defining a function in javascript and calling it in python
```javascript
function addTwo(x) {
    return x + 2;
}

module.exports = { addTwo };
```

```python
from javascript_module import addTwo

def test_addTwo():
    assert addTwo(2) == 4
```


## Function calling across cells with async functions
### This demonstrates an async function in javascript being run by our executor
```javascript
function js_sleep(ms) {
    return new Promise(resolve => setTimeout(resolve, ms));
}

async function js_sum(name, numbers) {
    let total = 0;
    for (const number of numbers) {
        await js_sleep(1000); // Sleep for 1 second
        total += number;
    }
    return total;
}

test('js_sum async function', async () => {
    const resultsPromise = Promise.all([
        js_sum("A", [1, 2]),
        js_sum("B", [1, 2, 3]),
    ]);
    await expect(resultsPromise).resolves.toEqual([3, 6]);
});
```

### This demonstrates an async function in python being run by our executor
```python
import asyncio
import unittest

async def py_sleep():
    await asyncio.sleep(1)

async def py_sum(name, numbers):
    total = 0
    for number in numbers:
        await py_sleep()
        total += number
    return total

class TestAsyncSum(unittest.IsolatedAsyncioTestCase):
    async def test_py_sum(self):
        tasks = [
            asyncio.create_task(py_sum("A", [1, 2])),
            asyncio.create_task(py_sum("B", [1, 2, 3])),
        ]
        results = await asyncio.gather(*tasks)
        self.assertEqual(results, [3, 6])

if __name__ == '__main__':
    unittest.main()
```