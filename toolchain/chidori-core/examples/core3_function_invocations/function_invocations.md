# This should demonstrate a variety of function calling interactions between different cell types and configurations

## Demonstrates defining a function in python and calling it in javascript
```python
def add_two(x):
    return x + 2
```

```javascript
const js_y = add_two(2);
```

## Demonstrates defining a function in javascript and calling it in python
```javascript
function addTwo(x) {
    return x + 2;
}
```

```python
py_y = addTwo(2)
```


## This demonstrates an async function in javascript being run by our executor
```javascript
function js_sleep(ms) {
    return new Promise(resolve => setTimeout(resolve, ms));
}

async function js_sum(name, numbers) {
    let total = 0;
    for (const number of numbers) {
        console.log(`Task ${name}: Computing ${total}+${number}`);
        await js_sleep(1000); // Sleep for 1 second
        total += number;
    }
}

async function main() {
    await Promise.all([
        js_sum("A", [1, 2]),
        js_sum("B", [1, 2, 3]),
    ]);
}
```

## This demonstrates an async function in python being run by our executor
```python
import asyncio
import time

start = time.time()

async def py_sleep():
    time.sleep(1)
    
async def py_sum(name, numbers):
    total = 0
    for number in numbers:
        print(f'Task {name}: Computing {total}+{number}')
        await py_sleep()
        total += number


loop = asyncio.get_event_loop()
tasks = [
    loop.create_task(py_sum("A", [1, 2])),
    loop.create_task(py_sum("B", [1, 2, 3])),
]
loop.run_until_complete(asyncio.wait(tasks))
loop.close()
```